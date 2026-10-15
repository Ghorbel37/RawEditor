/// WGSL shader code for real-time RAW image processing
///
/// This shader applies non-destructive edits to RAW sensor data in real-time.
/// Full debayering, color science, and advanced adjustments.
///
/// Color pipeline order (scene-referred → display-referred):
///   1.  Debayer          – camera-native linear RGB
///   2.  White Balance    – camera-space per-channel multipliers
///   3.  Color Matrix     – camera → sRGB linear (Bradford D50→D65 adapted)
///   4.  (Noise reduction runs before this pass — see gpu/denoise.rs)
///   5.  Sharpening       – unsharp mask in sRGB linear
///   6.  Temperature/Tint – blue-yellow and green-magenta axes in sRGB space
///   7.  Highlight clamp  – neutralise near-clipping channels before exposure,
///       then Dehaze      – invert the haze model (camera space, scene-linear)
///   8.  Exposure         – linear stop adjustment, × Texture/Clarity gain
///   9.  Highlights/Shadows
///  10.  Contrast         – pivoted at 18% gray (perceptual midtone)
///  11.  Levels           – whites / blacks
///  12.  Saturation
///  13.  Vibrance
///  14.  Filmic tone curve (fallback path only — DCP ProfileToneCurve otherwise)
///  15.  sRGB TRC         – IEC 61966-2-1 piecewise (γ=2.4 + linear toe)
///  16.  Clamp
pub const PASSTHROUGH_SHADER: &str = r#"
// ── Highlights / Shadows tuning ──────────────────────────────────────────────
// These operate in SCENE-LINEAR space, where values above 1.0 are real (WB
// amplification plus Step 3's highlight reconstruction routinely reach ~2).
// The knee range is therefore in scene-linear units: HL_KNEE_LO is "bright",
// HL_KNEE_HI is "well past clipping".
const HL_KNEE_LO: f32 = 0.5;
const HL_KNEE_HI: f32 = 1.5;
// At highlights = -1 with full weight the pixel scales by 1 - 0.6 = 0.4, which
// maps a 2.0 scene-linear highlight to 0.8 — back into the part of the tone
// curve that still resolves gradation.
const HL_RECOVER_STRENGTH: f32 = 0.6;
const HL_LIFT_STRENGTH: f32 = 0.4;
// Shadows fades out by SH_RANGE. This is a SCENE-LINEAR threshold, so it is far
// lower than the 0.5 that was correct when this ran display-referred: linear
// 0.18 is middle grey (~0.46 sRGB), whereas linear 0.5 is already ~0.74 sRGB
// and would drag most of the frame. Measured on _DSC0177: at 0.5 a shadows of
// only +0.25 lifted the whole-image mean 53.7 → 75.5, which is a global
// brightness control, not a shadow one.
const SH_RANGE: f32 = 0.18;
// Reduced from 0.15 for the same reason: an additive lift applied before the
// tone curve gets expanded by it, so it lands much harder than it used to.
const SH_LIFT_STRENGTH: f32 = 0.07;

// ========== Vertex Shader ==========
// Full-screen triangle (no vertex buffers needed)

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var output: VertexOutput;

    // Full-screen triangle covering entire viewport
    // Vertex 0: (-1, -1) -> tex (0, 1)
    // Vertex 1: ( 3, -1) -> tex (2, 1)
    // Vertex 2: (-1,  3) -> tex (0, -1)
    let x = f32(i32(vertex_index & 1u) * 4 - 1);
    let y = f32(i32(vertex_index >> 1u) * 4 - 1);

    output.clip_position = vec4<f32>(x, -y, 0.0, 1.0);

    // Map the target across `view_rect` — the sub-region of the displayed
    // image this pass is asked to produce. At (0,0,1,1) this is the identity
    // and the pass covers the whole image, which is what the histogram pass
    // and every un-zoomed display render use.
    //
    // Rendering only the visible window is what keeps cost flat as the user
    // zooms in: the target no longer grows with zoom, it just looks at a
    // smaller slice of the source at higher magnification.
    //
    // (params.zoom / pan_x / pan_y are vestigial and intentionally unread —
    // the viewport shader's projection matrix owns zoom and pan.)
    let t = vec2<f32>((x + 1.0) * 0.5, (y + 1.0) * 0.5);
    let view = params.view_rect.xy + t * params.view_rect.zw;

    // Phase 135: Conditional Crop
    // When is_cropping is active, we render the full image bounds [0, 1]
    // allowing the user to see and adjust the crop handles non-destructively.
    if (params.is_cropping == 0u) {
        output.tex_coords = params.crop.xy + view * params.crop.zw;
    } else {
        output.tex_coords = view;
    }

    return output;
}

// ========== Fragment Shader ==========

struct EditParams {
    exposure: f32,        // -5.0 to +5.0 stops
    contrast: f32,        // -100.0 to +100.0
    highlights: f32,      // -100.0 to +100.0
    shadows: f32,         // -100.0 to +100.0
    whites: f32,          // white point (default 1.0)
    blacks: f32,          // black point (default 0.0)
    vibrance: f32,        // -100.0 to +100.0
    saturation: f32,      // -100.0 to +100.0
    temperature: f32,     // -1.0 to +1.0
    tint: f32,            // -1.0 to +1.0
    padding1: f32,
    padding2: f32,
    // Phase 14: Color science metadata
    wb_multipliers: vec4<f32>,  // White balance [R, G, B, G2]
    // Camera RGB → working space, ROWS (rebuilt via transpose() below).
    // has_dcp == 1 → camera → ProPhoto (XYZ D50 step folded in on the CPU)
    // has_dcp == 0 → camera → linear sRGB
    forward_matrix_0: vec3<f32>,  // row 0
    padding3: f32,
    forward_matrix_1: vec3<f32>,  // row 1
    padding4: f32,
    forward_matrix_2: vec3<f32>,  // row 2
    padding5: f32,
    // Phase 25: Zoom & Pan
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    // Phase 34: CFA Pattern
    cfa_pattern: u32,

    // Sub-region of the displayed image this render covers: (u0, v0, du, dv)
    // in view UV. (0,0,1,1) = the whole image. Occupies four formerly-dead
    // padding floats, so offsets are unchanged. See gpu/params.rs.
    view_rect: vec4<f32>,

    // Phase 36: Per-channel Black Levels (vec4 alignment = 16 bytes)
    black_levels: vec4<u32>,

    white_level: u32,

    // Padding to reach 176 bytes
    pad_end_1: f32,
    pad_end_2: f32,
    pad_end_3: f32,

    // Phase 38: Manual Black Level Offsets
    black_offsets: vec4<f32>,

    // Phase 39: Black Level Phase Correction
    black_phase_x: u32,
    black_phase_y: u32,

    // Phase 133: Noise Reduction
    luma_noise: f32,
    color_noise: f32,

    // Phase 50: Sharpening
    sharpening: f32,

    // Phase 51: Sharpening Masking
    sharpen_masking: f32,

    // Phase 52: Rotation
    rotation: f32,

    // Padding to reach 224 bytes
    pad_phase_1: f32,

    // Phase 66: Crop (vec4 alignment = 16 bytes)
    crop: vec4<f32>,

    // Phase 135: Non-destructive crop visibility
    is_cropping: u32,

    // Padding to reach 256 bytes (16-byte alignment)
    has_dcp: u32,
    dcp_has_curve: u32,
    // EXIF orientation: 1 normal, 3 = 180°, 6 = 90° CW, 8 = 270° CW.
    // Display-space UVs are mapped to sensor space at the top of fs_main.
    orientation: u32,

    // Local adjustment masks (Step 10.5). Only the first mask_count are active.
    mask_count: u32,
    // Selected tone mapping operator. Only Camera (0) vs non-Camera is read;
    // the operator's shape arrives baked into the tone curve LUT. Occupies a
    // formerly-dead pad slot, so no offset moved. See gpu/params.rs.
    tone_mapper: u32,
    // 1 = sample the baked point-curve LUT (binding 6) in display space.
    curve_active: u32,
    // 0 = off, i + 1 = tint mask i's coverage red (see the end of fs_main).
    mask_overlay: u32,
    // Lens correction — see core::lens and Step 0. Mirrors gpu/params.rs.
    lens_dist: vec4<f32>,      // r_d/r_u polynomial p0..p3
    lens_misc: vec4<f32>,      // p4, scale.x, scale.y, active
    lens_tca_r: vec4<f32>,     // red (v, c, b), vignetting radius ratio
    lens_tca_b: vec4<f32>,     // blue (v, c, b), manual vignetting amount
    lens_vig: vec4<f32>,       // PA k1..k3, 1 / output corner radius
    defringe: vec4<f32>,       // purple amount, purple hue°, green amount, green hue°
    defringe_misc: vec4<f32>,  // active, edge radius (texels), -, -
    presence: vec4<f32>,       // texture, clarity, dehaze, -
    haze: vec4<f32>,           // airlight (camera-native rgb), haze map present
}

// One local adjustment mask (112 bytes — uniform array stride must be ×16).
// Mirrors gpu/params.rs GpuMask. Geometry lives in full-image display UV, the
// same space as params.crop, so masks stay glued to image content when the
// crop changes. Like the crop rectangle, masks do not follow the straighten
// rotation.
struct Mask {
    info: vec4<f32>,  // x: type (0 linear / 1 radial / 2 brush / 3 range), y: enabled, z: invert, w: feather
    geom: vec4<f32>,  // linear: ax, ay, bx, by;  radial: cx, cy, rx, ry
    adj0: vec4<f32>,  // exposure (EV), contrast, saturation, warmth
    adj1: vec4<f32>,  // highlights, shadows, tint, rotation (radians)
    adj2: vec4<f32>,  // hue (radians), has brush layer, luma range on, colour range on
    range: vec4<f32>, // luma lo, luma hi, luma smooth, colour tolerance
    color: vec4<f32>, // colour target (ln r/g, ln b/g), -, -
}

struct MaskBlock {
    masks: array<Mask, 8>,
}

// Group 1: local masks and the brush layers they paint into (gpu/brush.rs).
@group(1) @binding(0)
var brush_layers: texture_2d_array<f32>;

@group(1) @binding(1)
var<uniform> mask_block: MaskBlock;

// Group 2: Presence, all in sensor UV. The local-contrast bands
// (gpu/local_contrast.rs): y = B0 (fine), z = B1 (texture base), log2 luma;
// the Clarity guided-filter coefficients, B2 = a·B1 + b, on a coarse grid;
// and the dehaze guided-filter coefficients (core/haze.rs), (a, b) per
// coarse cell — Rg32Float, which is not filterable, hence the manual
// bilinear in haze_coeffs.
@group(2) @binding(0)
var local_tex: texture_2d<f32>;

@group(2) @binding(1)
var haze_tex: texture_2d<f32>;

@group(2) @binding(2)
var clarity_tex: texture_2d<f32>;

// Texture / Clarity: stops of gain per stop of band detail at +1 (at −1 the
// band is removed outright, i.e. ×1), and the soft ceiling on the total.
const TEXTURE_GAIN: f32 = 1.0;
const CLARITY_GAIN: f32 = 1.5;
const LOCAL_LIMIT_EV: f32 = 1.5;
// Dehaze: share of the estimated haze removed at +1 (He et al.'s ω — a
// little is kept so distance still reads), the transmission floor (caps the
// 1/t gain, which also multiplies noise), and how much haze −1 adds.
const DEHAZE_OMEGA: f32 = 0.95;
const DEHAZE_T0: f32 = 0.15;
const HAZE_ADD: f32 = 0.6;
// Sky tolerance (Wang et al.'s K, relative to the airlight): a pixel within
// this of the airlight is mostly haze-coloured scene — sky — and its 1/t
// gain is cut in proportion, or any error in A (and the sky's noise) comes
// back multiplied by up to 1/T0 as a colour cast.
const DEHAZE_SKY_K: f32 = 0.15;
// No pixel may lose more than this share of its darkest (airlight-relative)
// channel: removing more haze than the pixel itself contains drives it
// negative, which clamps — crushed, hue-shifted shadows.
const DEHAZE_KEEP: f32 = 0.9;

// Bilinear (a, b) of the dehaze guided filter at sensor UV.
fn haze_coeffs(uv: vec2<f32>) -> vec2<f32> {
    let dims = vec2<i32>(textureDimensions(haze_tex));
    let p = uv * vec2<f32>(dims) - vec2<f32>(0.5);
    let i0 = vec2<i32>(floor(p));
    let f = p - floor(p);
    let hi = dims - vec2<i32>(1);
    let c00 = textureLoad(haze_tex, clamp(i0, vec2<i32>(0), hi), 0).xy;
    let c10 = textureLoad(haze_tex, clamp(i0 + vec2<i32>(1, 0), vec2<i32>(0), hi), 0).xy;
    let c01 = textureLoad(haze_tex, clamp(i0 + vec2<i32>(0, 1), vec2<i32>(0), hi), 0).xy;
    let c11 = textureLoad(haze_tex, clamp(i0 + vec2<i32>(1, 1), vec2<i32>(0), hi), 0).xy;
    return mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y);
}

// Log-chromaticity epsilon for colour-range masks. MIRRORED from
// core::types::MASK_CHROMA_EPS — `shader_mask_constants_match_rust` checks.
const MASK_CHROMA_EPS: f32 = 0.004;

// Phase 128: Pass 2 now reads the debayered Rgba16Float intermediate texture.
@group(0) @binding(0)
var input_texture: texture_2d<f32>;  // Debayered camera-native linear RGB

@group(0) @binding(1)
var texture_sampler: sampler;  // Not used for integer textures, kept for compatibility

@group(0) @binding(2)
var<uniform> params: EditParams;

// Phase 140: DCP Textures
@group(0) @binding(3)
var hsv_lut: texture_3d<f32>;

@group(0) @binding(4)
var tone_curve: texture_1d<f32>;

@group(0) @binding(5)
var lut_sampler: sampler;

// Point curves, baked on the CPU (core::curves): rgb = R∘M, G∘M, B∘M.
@group(0) @binding(6)
var curve_lut: texture_1d<f32>;

fn sample_curve(x: f32) -> vec4<f32> {
    let n = f32(textureDimensions(curve_lut));
    let c = (clamp(x, 0.0, 1.0) * (n - 1.0) + 0.5) / n;
    return textureSampleLevel(curve_lut, lut_sampler, c, 0.0);
}

// Phase 128: Debayer functions removed — Pass 2 reads pre-debayered float texture.
// Neighbour helper for NR / sharpening.  One texture read per neighbour
// instead of the 9 that debayer() required.
fn get_neighbor(coords: vec2<i32>) -> vec3<f32> {
    let dimensions = textureDimensions(input_texture);
    let clamped = clamp(coords, vec2<i32>(0, 0), vec2<i32>(i32(dimensions.x) - 1, i32(dimensions.y) - 1));
    return textureLoad(input_texture, clamped, 0).rgb;
}

fn get_srgb_neighbor(coords: vec2<i32>, dimensions: vec2<u32>, cm: mat3x3<f32>) -> vec3<f32> {
    let clamped = vec2<i32>(
        clamp(coords.x, 0, i32(dimensions.x) - 1),
        clamp(coords.y, 0, i32(dimensions.y) - 1)
    );
    let cam = textureLoad(input_texture, clamped, 0).rgb;
    return max(cm * (cam * params.wb_multipliers.rgb), vec3<f32>(0.0));
}

// Fast per-pixel luma from the debayered camera texture.
// Applies WB but skips the full color matrix — green-dominant weighting
// approximates luminance in Bayer-sensor space and is ~5× cheaper per tap
// than get_srgb_neighbor (no mat3×3 multiply).
fn get_cam_luma(coords: vec2<i32>, dimensions: vec2<u32>) -> f32 {
    let clamped = clamp(coords, vec2<i32>(0), vec2<i32>(dimensions) - vec2<i32>(1));
    let cam = textureLoad(input_texture, clamped, 0).rgb * params.wb_multipliers.rgb;
    return cam.r * 0.25 + cam.g * 0.50 + cam.b * 0.25;
}

// Weight of a local adjustment mask at point p (full-image display UV,
// the same space as params.crop). Closed-form — no mask texture needed.
// `aspect` is the display (post-orientation) width/height ratio: UV space
// isn't square-pixel, so the radial branch's rotation must un-distort to an
// isotropic frame before rotating, or a rotated ellipse would come out
// sheared instead of rotated on any non-square image.
fn mask_shape(m: Mask, p: vec2<f32>, aspect: f32) -> f32 {
    var w: f32;
    if (m.info.x > 2.5) {
        // Range mask: the whole frame, narrowed only by its refinements.
        w = 1.0;
    } else if (m.info.x > 1.5) {
        // Brush mask: nothing but what is painted.
        w = 0.0;
    } else if (m.info.x < 0.5) {
        // Linear gradient: weight 1 at A falling smoothly to 0 at B.
        // Iso-lines are perpendicular to B−A in UV space.
        let d = m.geom.zw - m.geom.xy;
        let t = dot(p - m.geom.xy, d) / max(dot(d, d), 1e-6);
        w = 1.0 - smoothstep(0.0, 1.0, t);
    } else {
        // Radial ellipse with per-axis radii; feather sets where the
        // falloff starts (1 = from center, 0 = hard edge).
        let d = p - m.geom.xy;
        let cos_r = cos(-m.adj1.w);
        let sin_r = sin(-m.adj1.w);
        // Un-distort to isotropic space, apply the inverse rotation (maps
        // the sample point into the ellipse's unrotated local frame), then
        // redistort back before comparing against the (raw UV) radii. At
        // rotation=0 this reduces exactly to the un-rotated formula.
        let iso = vec2<f32>(d.x * aspect, d.y);
        let rotated_iso = vec2<f32>(
            iso.x * cos_r - iso.y * sin_r,
            iso.x * sin_r + iso.y * cos_r,
        );
        let rotated = vec2<f32>(rotated_iso.x / aspect, rotated_iso.y);
        let q = rotated / max(m.geom.zw, vec2<f32>(1e-4));
        let r = length(q); // 1.0 at the ellipse edge
        let inner = 1.0 - clamp(m.info.w, 0.0, 0.999);
        w = 1.0 - smoothstep(inner, 1.0, r);
    }
    return w;
}

// Full weight of mask `idx` at `p`: shape, plus its brush layer (signed:
// painting adds, erasing subtracts), times its range refinements, then
// inverted if asked. `color0` is the pixel as it entered the mask stage
// (linear sRGB) and `cam0` its camera-native value, for the luminance and
// colour ranges respectively.
fn mask_weight(m: Mask, idx: u32, p: vec2<f32>, aspect: f32, color0: vec3<f32>, cam0: vec3<f32>) -> f32 {
    var w = mask_shape(m, p, aspect);
    if (m.adj2.y > 0.5) {
        w = w + textureSampleLevel(brush_layers, lut_sampler, p, i32(idx), 0.0).r;
    }
    w = clamp(w, 0.0, 1.0);
    if (m.adj2.z > 0.5) {
        // Luminance band in display terms, where a photographer reads it.
        let lum = linear_to_srgb(max(dot(color0, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0));
        let s = m.range.z;
        w = w * smoothstep(m.range.x - s, m.range.x, lum) * (1.0 - smoothstep(m.range.y, m.range.y + s, lum));
    }
    if (m.adj2.w > 0.5) {
        // Exposure-invariant chromaticity of the camera-native colour.
        let c = max(cam0, vec3<f32>(0.0)) + vec3<f32>(MASK_CHROMA_EPS);
        let lc = vec2<f32>(log(c.r / c.g), log(c.b / c.g));
        let d = distance(lc, m.color.xy);
        let tol = 0.04 + 0.5 * m.range.w;
        w = w * (1.0 - smoothstep(tol, tol * 1.5 + 0.03, d));
    }
    if (m.info.z > 0.5) {
        w = 1.0 - w;
    }
    return w;
}

// Rotate a colour about the grey axis (Rodrigues): changes hue, keeps the
// channel mean.
fn hue_rotate(c: vec3<f32>, a: f32) -> vec3<f32> {
    let k = vec3<f32>(0.57735027);
    let ca = cos(a);
    return c * ca + cross(k, c) * sin(a) + k * dot(k, c) * (1.0 - ca);
}

// Top of the scene-linear domain the tone LUT covers.
// MIRRORED from core::tonemap::TONE_LUT_MAX — the Rust test
// `shader_tone_lut_constants_match_rust` asserts these agree.
const TONE_LUT_MAX: f32 = 16.0;

// Tone mapper ids. Only Camera is distinguished here: every other operator's
// SHAPE arrives baked into the curve LUT, so the shader needs no per-operator
// branch and no per-operator cost. See core::tonemap.
const TONE_CAMERA: u32 = 0u;

// Scene-linear → tone LUT coordinate. Log-encoded so one 4096-entry table can
// span [0, TONE_LUT_MAX] while still spending ~1000 entries below 1.0, where
// tone curves are steepest. Must match core::tonemap::tone_lut_encode exactly,
// or the baked table is read at the wrong positions.
fn tone_lut_encode(x: f32) -> f32 {
    return log2(1.0 + max(x, 0.0)) / log2(1.0 + TONE_LUT_MAX);
}

// Sample the tone curve at texel centers.
// The curve texture stores N samples of f(decode(i/(N-1))); naive normalized-
// coordinate sampling is skewed by half a texel at both ends.
fn sample_tone_curve(x: f32) -> f32 {
    let n = f32(textureDimensions(tone_curve));
    let t = clamp(tone_lut_encode(x), 0.0, 1.0);
    let c = (t * (n - 1.0) + 0.5) / n;
    return textureSampleLevel(tone_curve, lut_sampler, c, 0.0).r;
}

// Highlights & Shadows, ratio-preserving.
//
// `level` is the scalar the weights key off, supplied by the CALLER rather than
// derived here, because the right choice differs by colour space:
//   • linear sRGB (no-DCP fallback) → luma, `dot(c, (0.2126, 0.7152, 0.0722))`
//   • ProPhoto (DCP path)           → the PEAK CHANNEL
// ProPhoto's Y coefficients are (0.2880, 0.7119, 0.0001) — blue contributes
// essentially nothing, so a blown blue sky would score near-zero luma and the
// highlight weight would never engage. The peak channel is also the more
// meaningful quantity for recovery: what matters is proximity to clipping, not
// perceived brightness.
//
// Highlights scale the whole pixel so colour ratios survive — partially
// overexposed areas (warm-white cloud at R>G>B) stay warm instead of drifting
// neutral. A pixel whose channels ALL clipped has no ratio left to preserve and
// still goes neutral; that is a sensor limit, not a pipeline one.
//
// MUST run in scene-linear space, before any shoulder. Applied afterwards it is
// just a uniform multiply on an already-compressed band: the highlight goes
// grey and no texture comes back.
fn apply_highlights_shadows(c: vec3<f32>, highlights: f32, shadows: f32, level: f32) -> vec3<f32> {
    var color = c;
    var lv = level;
    if (highlights != 0.0) {
        // 0 below HL_KNEE_LO (midtones untouched), 1 above HL_KNEE_HI
        // (well past clipping).
        let hl_weight = smoothstep(HL_KNEE_LO, HL_KNEE_HI, lv);
        // Recovery gets more authority than lift: pulling a blown highlight
        // back has further to travel than brightening one.
        let strength = select(HL_LIFT_STRENGTH, HL_RECOVER_STRENGTH, highlights < 0.0);
        let factor = max(1.0 + highlights * strength * hl_weight, 0.2);
        color = color * factor;
        // Shadows keys off the POST-highlight level so the two compose, which
        // is what the original inline code did. Both luma and peak-channel are
        // homogeneous of degree 1 in `color`, so tracking the uniform scale
        // reproduces either exactly — no need to know which the caller passed.
        lv = lv * factor;
    }
    if (shadows != 0.0) {
        let sh_weight = clamp(1.0 - lv / SH_RANGE, 0.0, 1.0);
        color = color + vec3<f32>(sh_weight * shadows * SH_LIFT_STRENGTH);
    }
    return color;
}

// Adobe DNG SDK "RGBTone": hue-preserving tone curve application.
// Applying a steep s-curve independently per channel widens the spread between
// channels and badly oversaturates colours (neon reds/cyans/pinks).  Adobe
// instead curves the max and min channels and places the middle channel by
// linear interpolation, preserving the hue ratio (g-b)/(r-b) etc.
fn dcp_rgb_tone(c: vec3<f32>) -> vec3<f32> {
    let r = c.r; let g = c.g; let b = c.b;
    var rr: f32; var gg: f32; var bb: f32;
    if (r >= g) {
        if (g > b) {
            // r >= g > b
            rr = sample_tone_curve(r);
            bb = sample_tone_curve(b);
            gg = bb + (rr - bb) * (g - b) / (r - b);
        } else if (b > r) {
            // b > r >= g
            bb = sample_tone_curve(b);
            gg = sample_tone_curve(g);
            rr = gg + (bb - gg) * (r - g) / (b - g);
        } else if (b > g) {
            // r >= b > g
            rr = sample_tone_curve(r);
            gg = sample_tone_curve(g);
            bb = gg + (rr - gg) * (b - g) / (r - g);
        } else {
            // r >= g == b (neutral-ish)
            rr = sample_tone_curve(r);
            gg = sample_tone_curve(g);
            bb = gg;
        }
    } else {
        if (r >= b) {
            // g > r >= b
            gg = sample_tone_curve(g);
            bb = sample_tone_curve(b);
            rr = bb + (gg - bb) * (r - b) / (g - b);
        } else if (b > g) {
            // b > g > r
            bb = sample_tone_curve(b);
            rr = sample_tone_curve(r);
            gg = rr + (bb - rr) * (g - r) / (b - r);
        } else {
            // g >= b > r
            gg = sample_tone_curve(g);
            rr = sample_tone_curve(r);
            bb = rr + (gg - rr) * (b - r) / (g - r);
        }
    }
    return vec3<f32>(rr, gg, bb);
}

// IEC 61966-2-1 sRGB transfer function.
// Uses γ = 2.4 with a linear toe segment, NOT the 1/2.2 approximation.
// The difference is ~5% in the shadows and midtones.
fn linear_to_srgb(x: f32) -> f32 {
    if x <= 0.0031308 {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Discard fragments outside texture bounds (letterboxing when zoomed out)
    if input.tex_coords.x < 0.0 || input.tex_coords.x > 1.0 ||
       input.tex_coords.y < 0.0 || input.tex_coords.y > 1.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // Phase 52: Apply straightening rotation in DISPLAY space.
    // The aspect used for the angle correction must be the DISPLAY aspect —
    // for 90°/270° EXIF orientations that's the sensor aspect inverted.
    var tex_coords = input.tex_coords;

    if (abs(params.rotation) > 0.01) {
        let dimensions_r = textureDimensions(input_texture);
        var aspect = f32(dimensions_r.x) / f32(dimensions_r.y);
        if (params.orientation == 6u || params.orientation == 8u) {
            aspect = 1.0 / aspect;
        }
        let angle_rad = params.rotation * 3.14159265359 / 180.0;
        let cos_a = cos(angle_rad);
        let sin_a = sin(angle_rad);

        var centered = tex_coords - vec2<f32>(0.5, 0.5);
        centered.x *= aspect;

        let rotated = vec2<f32>(
            centered.x * cos_a - centered.y * sin_a,
            centered.x * sin_a + centered.y * cos_a
        );

        var unscaled = rotated;
        unscaled.x /= aspect;
        tex_coords = unscaled + vec2<f32>(0.5, 0.5);

        if (tex_coords.x < 0.0 || tex_coords.x > 1.0 ||
            tex_coords.y < 0.0 || tex_coords.y > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }

    // EXIF orientation: map display-space UV → sensor-space UV. The debayered
    // texture stays in native (landscape) sensor layout; the render target is
    // sized to the ORIENTED aspect by the caller, so sampling through this
    // mapping produces an upright image.
    //   6 = 90° CW display rotation:  sensor = (v, 1-u)
    //   8 = 270° CW (90° CCW):        sensor = (1-v, u)
    //   3 = 180°:                     sensor = (1-u, 1-v)
    if (params.orientation == 6u) {
        tex_coords = vec2<f32>(tex_coords.y, 1.0 - tex_coords.x);
    } else if (params.orientation == 8u) {
        tex_coords = vec2<f32>(1.0 - tex_coords.y, tex_coords.x);
    } else if (params.orientation == 3u) {
        tex_coords = vec2<f32>(1.0 - tex_coords.x, 1.0 - tex_coords.y);
    }

    let dimensions = textureDimensions(input_texture);

    // ── Step 0: Lens correction ──────────────────────────────────────────────
    // `tex_coords` is now this OUTPUT pixel's position in sensor UV. Map it to
    // the SOURCE position it comes from (core::lens): geometry first, then
    // per-channel lateral CA on the distorted coordinate, exactly as lensfun's
    // models are defined. `lens_misc.yz` takes a UV offset from centre into
    // the profile's calibration units; the Constrain zoom is folded into the
    // polynomial on the CPU (core::lens::gpu_params), since a zoom in this
    // scale would cancel when converting back.
    var uv_r = tex_coords;
    var uv_b = tex_coords;
    var lens_gain = 1.0;
    let lens_on = params.lens_misc.w > 0.5;
    if (lens_on) {
        let lscale = params.lens_misc.yz;
        let p = (tex_coords - vec2<f32>(0.5)) * lscale;
        let ru = length(p);
        let dc = params.lens_dist;
        let f = dc.x + ru * (dc.y + ru * (dc.z + ru * (dc.w + ru * params.lens_misc.x)));
        let pd = p * f;
        let rd = length(pd);
        let fr = params.lens_tca_r.x + rd * (params.lens_tca_r.y + rd * params.lens_tca_r.z);
        let fb = params.lens_tca_b.x + rd * (params.lens_tca_b.y + rd * params.lens_tca_b.z);
        tex_coords = pd / lscale + vec2<f32>(0.5);
        uv_r = pd * fr / lscale + vec2<f32>(0.5);
        uv_b = pd * fb / lscale + vec2<f32>(0.5);

        // Profile vignetting (PA model) at the SOURCE radius, which lensfun
        // normalises to the half-diagonal.
        let rv = rd * params.lens_tca_r.w;
        let rv2 = rv * rv;
        let k = params.lens_vig.xyz;
        lens_gain = 1.0 / max(1.0 + rv2 * (k.x + rv2 * (k.y + rv2 * k.z)), 0.05);
        // Manual vignetting on the OUTPUT frame: ±1 = ±1 stop at the corner.
        let rn = ru * params.lens_vig.w;
        let rn2 = rn * rn;
        lens_gain *= exp2(params.lens_tca_b.w * (0.5 * rn2 + 0.5 * rn2 * rn2));

        if (tex_coords.x < 0.0 || tex_coords.x > 1.0 || tex_coords.y < 0.0 || tex_coords.y > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }

    let pixel_coords = vec2<i32>(
        i32(tex_coords.x * f32(dimensions.x)),
        i32(tex_coords.y * f32(dimensions.y))
    );

    // ── Step 1: Read debayered camera-native linear RGB from intermediate ────
    // Phase 135: textureSample with linear filtering for smooth downscaling.
    // With lens correction on, red and blue come from their own CA-shifted
    // positions (explicit LOD: these coordinates are not a smooth function of
    // the screen, so implicit derivatives would be meaningless).
    var color: vec3<f32>;
    if (lens_on) {
        color = vec3<f32>(
            textureSampleLevel(input_texture, texture_sampler, uv_r, 0.0).r,
            textureSampleLevel(input_texture, texture_sampler, tex_coords, 0.0).g,
            textureSampleLevel(input_texture, texture_sampler, uv_b, 0.0).b,
        );
    } else {
        color = textureSample(input_texture, texture_sampler, tex_coords).rgb;
    }
    // Camera-native colour, for colour-range masks (exposure- and
    // WB-independent by construction).
    let cam0 = color;

    // Phase 135: Non-destructive dimming
    // If we are in crop mode, dim the area outside the current crop rectangle.
    // Note: We use input.tex_coords (pre-rotation) to check against the crop bounds.
    if (params.is_cropping == 1u) {
        let in_x = input.tex_coords.x >= params.crop.x && input.tex_coords.x <= (params.crop.x + params.crop.z);
        let in_y = input.tex_coords.y >= params.crop.y && input.tex_coords.y <= (params.crop.y + params.crop.w);
        if (!(in_x && in_y)) {
            color = color * 0.3; // 70% dimming for discarded areas
        }
    }

    // (STAGE 1 clip detection removed — any per-Bayer-site approach creates a
    // 2-pixel-period checkerboard on bright fabric/sky because adjacent R/G/B Bayer
    // sites have structurally different raw values for any non-neutral scene.
    // STAGE 2 gamut compression handles overbrights post-matrix without artifacts.)

    // ── Step 2: White Balance ─────────────────────────────────────────────────
    color = color * params.wb_multipliers.rgb;

    // Pre-fetch the 8 nearest camera-space neighbours (WB-scaled) for the
    // sharpening ring. The branch is uniform across the warp (params are
    // constants), so no divergence.
    var nb_n  = vec3<f32>(0.0); var nb_s  = vec3<f32>(0.0);
    var nb_e  = vec3<f32>(0.0); var nb_w  = vec3<f32>(0.0);
    var nb_ne = vec3<f32>(0.0); var nb_nw = vec3<f32>(0.0);
    var nb_se = vec3<f32>(0.0); var nb_sw = vec3<f32>(0.0);
    if (params.sharpening > 0.0) {
        let wb3  = params.wb_multipliers.rgb;
        let dmax = vec2<i32>(dimensions) - vec2<i32>(1);
        nb_n  = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>( 0, -1), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_s  = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>( 0,  1), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_e  = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>( 1,  0), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_w  = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>(-1,  0), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_ne = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>( 1, -1), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_nw = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>(-1, -1), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_se = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>( 1,  1), vec2<i32>(0), dmax), 0).rgb * wb3;
        nb_sw = textureLoad(input_texture, clamp(pixel_coords + vec2<i32>(-1,  1), vec2<i32>(0), dmax), 0).rgb * wb3;
    }

    // (Noise reduction is no longer done here: it runs once per change in the
    // cached denoise pre-pass — gpu/denoise.rs — and this pass samples its
    // output. luma_noise / color_noise are vestigial in this struct.)

    // ── Step 3: Highlight neutralisation (camera space, per-channel) ──────────
    // When a raw sensor channel clips (hits digital saturation at raw = 1.0), its
    // WB-amplified value reaches wb_multipliers.channel × 1.0 = wb_multipliers.channel.
    // Adjacent Bayer-opposite pixels (e.g. a G site next to a clipped B sky pixel) have
    // the clipped channel interpolated from their neighbours by the debayer pass, so
    // color.b at the G site also approaches wb_multipliers.b.  Comparing each channel's
    // debayered value against its own WB multiplier therefore produces a spatially-smooth
    // blend weight — free of the 2-pixel Bayer-period checkerboard that appeared when
    // the old code used the per-Bayer-site raw alpha instead.
    //
    // This runs before the exposure step so the correction is visible when exposure is
    // pulled down (which brings clipped areas into the visible range as magenta).
    // ── Step 3: Highlight RECONSTRUCTION (camera space, per-channel) ─────────
    // A channel that hits sensor saturation reads its clip ceiling instead of
    // its true (higher) value; passed downstream unchanged that false ratio
    // shows as a colour cast (magenta sky = green clipped below red/blue's
    // true balance).  Instead of desaturating, ESTIMATE the clipped channel
    // from the unclipped ones: highlights are overwhelmingly near-neutral
    // after WB, so the clip-weighted average of the surviving channels is a
    // sound floor for the true value.  max() guarantees we only ever RAISE a
    // clipped channel — a bright red shirt (only red clipped, green/blue low)
    // keeps its measured red and stays red, while an overexposed sky (green
    // and blue clipped) is lifted to the unclipped channel's level, recovering
    // both hue and up to a stop of brightness that the ProPhoto shoulder then
    // rolls into visible gradation.
    //
    // Per-channel clip weights compare each DEBAYERED value against that
    // channel's own WB-amplified ceiling — spatially smooth, no Bayer-period
    // checkerboard (unlike raw per-site detection).
    let wb3 = params.wb_multipliers.rgb;
    let hl_clip_w = vec3<f32>(
        smoothstep(wb3.r * 0.90, wb3.r, color.r),
        smoothstep(wb3.g * 0.90, wb3.g, color.g),
        smoothstep(wb3.b * 0.90, wb3.b, color.b)
    );
    let hl_unclip = vec3<f32>(1.0) - hl_clip_w;
    let hl_den = hl_unclip.r + hl_unclip.g + hl_unclip.b;
    if (hl_den > 0.05) {
        let hl_est = dot(color * hl_unclip, vec3<f32>(1.0)) / hl_den;
        color = mix(color, max(color, vec3<f32>(hl_est)), hl_clip_w);
    } else {
        // All three channels clipped: hue unrecoverable — go neutral at the
        // brightest ceiling so the shoulder renders clean white.
        let hl_max = max(color.r, max(color.g, color.b));
        color = vec3<f32>(hl_max);
    }

    // ── Step 3b: Dehaze ───────────────────────────────────────────────────────
    // Haze is additive light: I = J·t + A·(1 − t), A the airlight. Inverted
    // here, on scene-linear camera values where that model is exact — after
    // highlight reconstruction (which needs the true clip ceilings) and
    // before exposure, which it commutes with. The dark channel of I/A reads
    // 1 − t (core::haze); evaluating the guided filter with this pixel's own
    // guide value is what keeps the skyline sharp. Normalising by A also
    // makes all of it independent of the WB sliders.
    let dehaze = params.presence.z;
    if (dehaze != 0.0 && params.haze.w > 0.5) {
        let air = params.haze.rgb * wb3;
        let nrm = color / max(air, vec3<f32>(1e-6));
        let ab = haze_coeffs(tex_coords);
        let dark = clamp(ab.x * (nrm.r + nrm.g + nrm.b) / 3.0 + ab.y, 0.0, 1.0);
        if (dehaze > 0.0) {
            var t = 1.0 - DEHAZE_OMEGA * dehaze * dark;
            let own_dark = min(nrm.r, min(nrm.g, nrm.b));
            t = max(t, 1.0 - DEHAZE_KEEP * own_dark);
            let from_air = max(abs(nrm.r - 1.0), max(abs(nrm.g - 1.0), abs(nrm.b - 1.0)));
            t = min(t * max(DEHAZE_SKY_K / max(from_air, 1e-4), 1.0), 1.0);
            t = max(t, DEHAZE_T0);
            color = max((color - air) / t + air, vec3<f32>(0.0));
        } else {
            // Adding haze: more where there already is some (distance),
            // a little everywhere.
            let t = 1.0 + HAZE_ADD * dehaze * (0.35 + 0.65 * dark);
            color = color * t + air * (1.0 - t);
        }
    }

    // ── Step 4: Exposure ──────────────────────────────────────────────────────
    // The lens vignetting gain rides here rather than at sampling: applied
    // before Step 3 it would push brightened corners past the clip thresholds
    // and trigger false highlight reconstruction.
    //
    // So do Texture and Clarity: each scales one band of log luminance
    // (gpu/local_contrast.rs), which is a per-pixel exposure change — hue
    // and saturation untouched, and scene-linear, so the shoulder below rolls
    // off whatever it lifts past white. Both fade out in deep shadows, where
    // log-domain detail is mostly noise; Clarity also leaves the highlights
    // alone, keeping it a midtone control.
    var local_ev = 0.0;
    let texture_amt = params.presence.x;
    let clarity_amt = params.presence.y;
    if (texture_amt != 0.0 || clarity_amt != 0.0) {
        let bands = textureSampleLevel(local_tex, texture_sampler, tex_coords, 0.0);
        let gf = textureSampleLevel(clarity_tex, texture_sampler, tex_coords, 0.0).xy;
        let b2 = gf.x * bands.z + gf.y;
        // Surrounding level after exposure, in stops below sensor white.
        let base_ev = b2 + params.exposure;
        let shadow_w = smoothstep(-11.0, -7.0, base_ev);
        let mid_w = shadow_w * (1.0 - smoothstep(-1.5, 1.0, base_ev));
        let k_tex = select(TEXTURE_GAIN, 1.0, texture_amt < 0.0);
        let k_clar = select(CLARITY_GAIN, 1.0, clarity_amt < 0.0);
        local_ev = texture_amt * k_tex * (bands.y - bands.z) * shadow_w
                 + clarity_amt * k_clar * (bands.z - b2) * mid_w;
        local_ev = local_ev / (1.0 + abs(local_ev) / LOCAL_LIMIT_EV);
    }
    let exposure_multiplier = pow(2.0, params.exposure) * lens_gain * exp2(local_ev);
    color = color * exposure_multiplier;

    // ── Step 5: Camera RGB → sRGB (DCP Pipeline vs Fallback) ────────────────
    // NOTE: WB was already applied in Step 2 above. Do NOT re-apply here.

    let matrix_from_params = transpose(mat3x3<f32>(
        params.forward_matrix_0,
        params.forward_matrix_1,
        params.forward_matrix_2
    ));

    if (params.has_dcp == 1u) {
        // 1. Camera RGB → ProPhoto RGB, in one multiply.
        // The DCP's ForwardMatrix is camera → XYZ D50, and XYZ D50 → ProPhoto is
        // a fixed matrix; both are constant across the frame, so their product
        // is composed once on the CPU (raw/dcp.rs::interpolate_at_temperature)
        // instead of traversing XYZ per pixel.
        //
        // No pre-clamp here — negative ProPhoto values (wide-gamut colors) must reach
        // the HueSatMap with correct hue/saturation. We clamp after reconstruction.
        var pp = matrix_from_params * color;

        // ── Highlights & Shadows (SCENE-LINEAR, before the shoulder) ────────
        // Deliberately here and not at Step 10. Everything below — the knee,
        // the HueSatMap value axis, the tone-curve LUT — works in [0,1], so by
        // Step 10 a full stop of highlight (1.0 → 2.0 scene-linear) has already
        // been squeezed by the knee into 0.850 → 0.944, about 9% of range.
        // Recovery applied to that is a uniform multiply on a flattened band:
        // the highlight goes grey and no texture returns. Running it here, on
        // values that still exceed 1.0, redistributes them back down into the
        // part of the curve that resolves detail.
        //
        // Peak channel rather than luma — see apply_highlights_shadows.
        let hs_level = max(pp.r, max(pp.g, pp.b));
        pp = apply_highlights_shadows(pp, params.highlights, params.shadows, hs_level);

        // Highlight shoulder — CAMERA OPERATOR ONLY.
        //
        // WB amplification pushes bright pixels above 1.0 linear (a bright blue
        // puck: raw 0.75 × wb_b 1.45 ≈ 1.09). The DCP ProfileToneCurve is
        // DEFINED only on [0,1], so without this every value > 1 would collapse
        // to one flat output: blown highlights with zero gradation. Compress
        // [0.7, ∞) smoothly into [0.7, 1.0) per channel instead (C1-continuous
        // Reinhard knee). Being per-channel is deliberate, not incidental: it
        // also lets bright pixels sample the profile's top value slices, where
        // camera-matching profiles desaturate/darken (this DCP scales saturated
        // cyan sat ×0.78 and red value ×0.65–0.85 up there).
        //
        // Every OTHER operator is itself the scene-linear → display mapping,
        // and its LUT spans [0, TONE_LUT_MAX]. Kneeing first would hand it an
        // already-compressed signal and it would tone-map a tone-mapped image —
        // the exact failure that made Highlights recovery return grey instead
        // of texture. So the knee is skipped and the operator sees the real
        // scene-linear range. (The HueSatMap's value axis still clamps at 1.0
        // for its LOOKUP, which is correct: it only selects a value slice.)
        if (params.tone_mapper == TONE_CAMERA) {
            let sh_knee: f32 = 0.7;
            let sh_head: f32 = 1.0 - sh_knee;
            if (pp.r > sh_knee) { let o = pp.r - sh_knee; pp.r = sh_knee + sh_head * o / (o + sh_head); }
            if (pp.g > sh_knee) { let o = pp.g - sh_knee; pp.g = sh_knee + sh_head * o / (o + sh_head); }
            if (pp.b > sh_knee) { let o = pp.b - sh_knee; pp.b = sh_knee + sh_head * o / (o + sh_head); }
        }

        // 3. ProPhoto HSV for HueSatMap lookup
        let v    = max(pp.r, max(pp.g, pp.b));
        let cmin = min(pp.r, min(pp.g, pp.b));
        let delta = v - cmin;
        var h = 0.0;
        var s = 0.0;
        if (v > 0.001) { s = delta / v; }
        if (delta > 0.001) {
            if (v == pp.r) {
                h = (pp.g - pp.b) / delta;
                if (h < 0.0) { h += 6.0; }
            } else if (v == pp.g) {
                h = (pp.b - pp.r) / delta + 2.0;
            } else {
                h = (pp.r - pp.g) / delta + 4.0;
            }
            h /= 6.0;
        }

        // Texel-center mapping for the HueSatMap LUT.
        // X axis holds hueDivs+1 texels (last duplicates hue 0 so the clamp
        // sampler interpolates across the 0°/360° red seam); sat and val axes
        // span their grid inclusively, so grid point i sits at texel center i.
        let lut_dims = vec3<f32>(textureDimensions(hsv_lut));
        let lut_uvw = vec3<f32>(
            (h * max(lut_dims.x - 1.0, 1.0) + 0.5) / lut_dims.x,
            (s * max(lut_dims.y - 1.0, 1.0) + 0.5) / lut_dims.y,
            (clamp(v, 0.0, 1.0) * max(lut_dims.z - 1.0, 1.0) + 0.5) / lut_dims.z
        );
        let lut_val = textureSampleLevel(hsv_lut, lut_sampler, lut_uvw, 0.0);
        h = fract(h + lut_val.r / 360.0);
        s = clamp(s * lut_val.g, 0.0, 1.0);
        let new_v = max(v * lut_val.b, 0.0);

        // HSV → ProPhoto RGB (s==0 degenerates correctly: p=q=t=new_v for all cases)
        var r_pp = 0.0; var g_pp = 0.0; var b_pp = 0.0;
        let h6  = h * 6.0;
        let hi  = floor(h6);
        let f   = h6 - hi;
        let p   = new_v * (1.0 - s);
        let q   = new_v * (1.0 - f * s);
        let t   = new_v * (1.0 - (1.0 - f) * s);
        let idx = i32(hi) % 6;
        if      (idx == 0) { r_pp = new_v; g_pp = t;     b_pp = p;     }
        else if (idx == 1) { r_pp = q;     g_pp = new_v; b_pp = p;     }
        else if (idx == 2) { r_pp = p;     g_pp = new_v; b_pp = t;     }
        else if (idx == 3) { r_pp = p;     g_pp = q;     b_pp = new_v; }
        else if (idx == 4) { r_pp = t;     g_pp = p;     b_pp = new_v; }
        else               { r_pp = new_v; g_pp = p;     b_pp = q;     }
        // Clamp after reconstruction — out-of-gamut negatives are clipped here, not before LUT
        pp = max(vec3<f32>(r_pp, g_pp, b_pp), vec3<f32>(0.0));

        // 4. ProfileToneCurve (linear 1:1 if no curve in this DCP)
        // Applied hue-preservingly (Adobe RGBTone) — see dcp_rgb_tone above.
        pp = dcp_rgb_tone(pp);

        // 5. ProPhoto → XYZ D50 → Bradford D50→D65 → linear sRGB
        // Combined matrix: xyz_to_srgb_D65 @ bradford_D50→D65 @ prophoto_to_xyz_D50
        // Column-major for WGSL (each vec3 is a column):
        let prophoto_to_srgb = mat3x3<f32>(
            2.0340758, -0.2288131, -0.0085698,
            -0.7273341,  1.2317301, -0.1532866,
            -0.3067418, -0.0029168,  1.1618564
        );
        color = prophoto_to_srgb * pp;

    } else {
        color = matrix_from_params * color;
    }

    // ── Step 6: Gamut-map negative channels (luma-preserving) ────────────────
    // Colours outside sRGB (deep cyans, magentas) come out of the matrix with a
    // negative channel.  Hard-clamping that channel to 0 shifts the colour to the
    // gamut boundary at MAXIMUM saturation — the "neon cone" artifact.  Instead
    // desaturate toward luma just enough to lift the most-negative channel to 0:
    // hue and luminance survive, only excess chroma is given up.
    let neg_luma = dot(max(color, vec3<f32>(0.0)), vec3<f32>(0.2126, 0.7152, 0.0722));
    let neg_min = min(color.r, min(color.g, color.b));
    if (neg_min < 0.0 && neg_luma > 0.0) {
        let t_neg = neg_luma / (neg_luma - neg_min);
        color = vec3<f32>(neg_luma) + (color - vec3<f32>(neg_luma)) * t_neg;
    }
    // Safety net for degenerate cases (all channels negative).
    color = max(color, vec3<f32>(0.0));

    // ── Step 6b: Defringe (axial CA) ─────────────────────────────────────────
    // Purple/green fringes sit beside high-contrast edges and are a property of
    // the lens's focus, not its geometry, so Step 0 cannot remove them.
    // Desaturate pixels whose hue falls in a fringe band AND that sit near a
    // strong local luma edge; flat coloured areas are left alone.
    if (params.defringe_misc.x > 0.5) {
        let rad = i32(params.defringe_misc.y);
        var lmax = get_cam_luma(pixel_coords, dimensions);
        var lmin = lmax;
        for (var dy = -1; dy <= 1; dy++) {
            for (var dx = -1; dx <= 1; dx++) {
                if (dx == 0 && dy == 0) { continue; }
                let l = get_cam_luma(pixel_coords + vec2<i32>(dx, dy) * rad, dimensions);
                lmax = max(lmax, l);
                lmin = min(lmin, l);
            }
        }
        let edge = smoothstep(0.15, 0.5, (lmax - lmin) / (lmax + 0.02));
        let cmax = max(color.r, max(color.g, color.b));
        let cmin = min(color.r, min(color.g, color.b));
        let chroma = cmax - cmin;
        if (edge > 0.0 && chroma > 1e-5) {
            var hue: f32;
            if (cmax == color.r) {
                hue = (color.g - color.b) / chroma;
            } else if (cmax == color.g) {
                hue = (color.b - color.r) / chroma + 2.0;
            } else {
                hue = (color.r - color.g) / chroma + 4.0;
            }
            hue = fract(hue / 6.0 + 1.0) * 360.0;
            let sat_w = smoothstep(0.08, 0.25, chroma / max(cmax, 1e-5));
            let dp = abs(fract((hue - params.defringe.y) / 360.0 + 0.5) - 0.5) * 360.0;
            let dg = abs(fract((hue - params.defringe.w) / 360.0 + 0.5) - 0.5) * 360.0;
            // Purple fringes run from blue-violet to magenta, hence the wider
            // band; green fringes are narrower in hue.
            let band = max(
                params.defringe.x * (1.0 - smoothstep(30.0, 60.0, dp)),
                params.defringe.z * (1.0 - smoothstep(25.0, 50.0, dg)),
            );
            let w = clamp(band * edge * sat_w, 0.0, 1.0);
            let lum = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
            color = mix(color, vec3<f32>(lum), w);
        }
    }

    // ── Phase 132: 5×5 Perceptual USM with Edge-Aware Masking ──────────────
    // All reads use get_cam_luma (WB + dot, no color matrix), which is ~5×
    // cheaper per tap than get_srgb_neighbor.  Using camera luma for both the
    // center and neighbours keeps the high-pass signal in a consistent space —
    // the old 3×3 code mixed post-exposure sRGB (center) with pre-exposure
    // camera (neighbours), biasing the signal at non-zero exposure.
    //
    // params.sharpen_masking: 0 = sharpen everything, 1 = edges only.
    // The Laplacian is free since n/s/e/w are already fetched for the blur.
    if (params.sharpening > 0.0) {
        // Center luma in camera space (pre-exposure, consistent with neighbours)
        let center_cam = textureLoad(input_texture, pixel_coords, 0).rgb * params.wb_multipliers.rgb;
        let c_luma_linear = max(0.0001, center_cam.r * 0.25 + center_cam.g * 0.50 + center_cam.b * 0.25);
        let c_luma = sqrt(c_luma_linear);

        // Radius-1 samples: derived from the pre-fetched nb_* ring (no extra reads)
        let clw = vec3<f32>(0.25, 0.50, 0.25);
        let n1  = sqrt(max(0.0001, dot(nb_n,  clw)));
        let s1  = sqrt(max(0.0001, dot(nb_s,  clw)));
        let e1  = sqrt(max(0.0001, dot(nb_e,  clw)));
        let w1  = sqrt(max(0.0001, dot(nb_w,  clw)));
        let ne1 = sqrt(max(0.0001, dot(nb_ne, clw)));
        let nw1 = sqrt(max(0.0001, dot(nb_nw, clw)));
        let se1 = sqrt(max(0.0001, dot(nb_se, clw)));
        let sw1 = sqrt(max(0.0001, dot(nb_sw, clw)));

        // Radius-2 axis samples
        let n2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 0, -2), dimensions)));
        let s2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 0,  2), dimensions)));
        let e2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 2,  0), dimensions)));
        let w2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-2,  0), dimensions)));

        // Radius-2 mixed samples (axis×2, diag×1)
        let n2e1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 1, -2), dimensions)));
        let n2w1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-1, -2), dimensions)));
        let s2e1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 1,  2), dimensions)));
        let s2w1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-1,  2), dimensions)));
        let e2n1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 2, -1), dimensions)));
        let e2s1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 2,  1), dimensions)));
        let w2n1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-2, -1), dimensions)));
        let w2s1 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-2,  1), dimensions)));

        // Radius-2 corner samples
        let ne2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 2, -2), dimensions)));
        let nw2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-2, -2), dimensions)));
        let se2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>( 2,  2), dimensions)));
        let sw2 = sqrt(max(0.0001, get_cam_luma(pixel_coords + vec2<i32>(-2,  2), dimensions)));

        // 5×5 Gaussian blur (σ≈1.0) in perceptual camera-luma space
        //  kernel weights: 41 26 7 / 273 centre→edge on each axis
        //  ┌ 1  4  7  4  1 ┐
        //  │ 4 16 26 16  4 │
        //  │ 7 26 41 26  7 │ / 273
        //  │ 4 16 26 16  4 │
        //  └ 1  4  7  4  1 ┘
        let blur_luma = (
              c_luma * 41.0
            + (n1 + s1 + e1 + w1) * 26.0
            + (ne1 + nw1 + se1 + sw1) * 16.0
            + (n2 + s2 + e2 + w2) * 7.0
            + (n2e1 + n2w1 + s2e1 + s2w1 + e2n1 + e2s1 + w2n1 + w2s1) * 4.0
            + (ne2 + nw2 + se2 + sw2) * 1.0
        ) / 273.0;

        let high_pass = c_luma - blur_luma;

        // Laplacian edge strength (free — cardinal neighbours already fetched).
        // High value = hard edge; low value = smooth/noisy flat area.
        let laplacian = abs(4.0 * c_luma - n1 - s1 - e1 - w1);

        // sharpen_masking=0 → mask=1 everywhere (no protection).
        // sharpen_masking=1 → smoothly ramps up: flat areas (laplacian<0.04)
        // get little or no sharpening, protecting noise from amplification.
        let mask = mix(1.0, smoothstep(0.0, 0.08, laplacian), params.sharpen_masking);

        let sharpened_perc = max(0.0, c_luma + high_pass * params.sharpening * 2.0 * mask);
        let new_linear_luma = sharpened_perc * sharpened_perc;

        // Ratio-preserving scale applied to the fully-processed sRGB colour.
        // The exposure factor cancels in new/old ratio, so camera-space luma
        // in the denominator is correct even though `color` has exposure baked in.
        let sharpen_scale = new_linear_luma / c_luma_linear;
        color = color * sharpen_scale;
    }

    // ── Step 10: Highlights & Shadows — FALLBACK PATH ONLY ───────────────────
    // On the DCP path this already ran in ProPhoto scene-linear, before the
    // shoulder (see the block above); running it again here would double-apply.
    //
    // The fallback path has no shoulder until the filmic curve at Step 15, and
    // `color` here is linear sRGB that still exceeds 1.0, so this position is
    // already scene-linear for that path — it needs no move.
    //
    // Gated on has_dcp, NOT dcp_has_curve: the knee runs for any DCP, curve or
    // not, so any DCP means the work was already done upstream.
    if (params.has_dcp == 0u) {
        let luma_hl = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
        color = apply_highlights_shadows(color, params.highlights, params.shadows, luma_hl);
    }

    // ── STAGE 2: Gamut Compression (True Path-to-White) ──────────────────────
    // Mix toward the peak channel value (neutral at that brightness).  This fixes
    // the magenta/colour cast that appears in partially-clipped sensor highlights:
    // when the blue raw channel clips before red and green, the wrong channel ratios
    // pass through the forward matrix and appear pink/magenta in sRGB.  Correcting
    // here (post-matrix) means the blend operates on spatially-smooth sRGB values
    // rather than per-Bayer-site camera values, so there is no checkerboard.
    //
    // Step 3 (camera space, pre-exposure) now handles sensor-clipping colour casts.
    // STAGE 2 remains as a gentle safety net for any post-matrix overbrights that
    // survive (e.g. extreme exposure boosts) — range (0, 2.0) is intentionally light
    // so legitimate saturated colours that are slightly over 1.0 are not desaturated.
    let max_c_gc = max(color.r, max(color.g, color.b));
    let overbright = max(0.0, max_c_gc - 1.0);
    let path_to_white = smoothstep(0.0, 2.0, overbright);
    color = mix(color, vec3<f32>(max_c_gc), path_to_white);

    // (Step 9 removed — Temperature & Tint are now REAL white balance: the CPU
    // solves the slider Kelvin/tint to camera WB multipliers via the colour
    // matrix (Robertson CCT, see src/color.rs) and updates the wb_multipliers
    // uniform. Post-matrix sRGB channel scaling twisted hues at extremes.)

    // ── Step 10.5: Local Adjustment Masks ────────────────────────────────────
    // Applied in sRGB-linear space, after global highlights/shadows + gamut
    // compression and before global contrast/saturation, so local adjustments
    // compose consistently with their global counterparts. Mask geometry is
    // evaluated against input.tex_coords (full-image display UV, pre-rotation)
    // — the same convention as the crop rectangle.
    //
    // UV→isotropic scale factor for mask_weight's rotation math. Starts from
    // the display (post-orientation) aspect — mirroring the straightening-
    // rotation block's orientation swap above, reusing the `dimensions`
    // variable already computed for Step 1.
    // This is the FULL frame's display aspect, with no crop term, and that is
    // correct for both crop mode and normal viewing. The render target is now
    // sized to the CROPPED aspect (see RawEditor::image_display_dims), so the
    // crop sub-rect is scaled UNIFORMLY onto it: one unit of u spans
    // target_w/crop.z pixels and one unit of v spans target_h/crop.w, and
    // those two ratios reduce to exactly the full frame's aspect.
    //
    // A crop term used to live here to cancel the non-uniform stretch the
    // old full-aspect target produced. Re-adding one would double-apply the
    // crop and shear rotated ellipses. Keep in sync with
    // core::viewport::mask_uv_aspect, the CPU side of this.
    var mask_aspect = f32(dimensions.x) / f32(dimensions.y);
    if (params.orientation == 6u || params.orientation == 8u) {
        mask_aspect = 1.0 / mask_aspect;
    }
    // Every mask's luminance range keys off the pixel as it ENTERED this
    // stage, so masks don't shift each other's selections.
    let color0 = color;
    var overlay_w = 0.0;
    for (var mi = 0u; mi < min(params.mask_count, 8u); mi = mi + 1u) {
        let m = mask_block.masks[mi];
        if (m.info.y < 0.5) {
            continue;
        }
        let w = mask_weight(m, mi, input.tex_coords, mask_aspect, color0, cam0);
        if (params.mask_overlay == mi + 1u) {
            overlay_w = w;
        }
        if (w < 0.001) {
            continue;
        }

        // Hue: rotate about the grey axis (±30° at ±1).
        if (m.adj2.x != 0.0) {
            color = mix(color, hue_rotate(color, m.adj2.x), w);
        }

        // Adjustment ranges: exposure ±4 EV; all others −1..1 (same units as
        // the global highlights/shadows sliders).

        // Exposure: weighted stops
        color = color * exp2(m.adj0.x * w);

        // Warmth: gentle sRGB-linear channel tilt (±10% at ±1). Real WB is
        // uniform-wide (can't vary per pixel), so this local tilt is kept
        // deliberately modest — large post-matrix channel scaling twists hues.
        let warm = m.adj0.w * w;
        if (warm != 0.0) {
            color = color * vec3<f32>(1.0 + 0.10 * warm, 1.0, 1.0 - 0.10 * warm);
        }

        // Tint: same gentle-tilt technique as warmth, on the green/magenta
        // axis instead of blue/amber. Positive = MAGENTA, matching the ACR
        // slider and this codebase's global tint (see TINT_SCALE in
        // src/color.rs) — the two tint sliders sit in the same sidebar and
        // must not run in opposite directions.
        let tint = m.adj1.z * w;
        if (tint != 0.0) {
            color = color * vec3<f32>(1.0 + 0.10 * tint, 1.0 - 0.10 * tint, 1.0 + 0.10 * tint);
        }

        // Highlights: same luma-weighted multiplicative form as Step 10
        let m_hl = m.adj1.x * w;
        if (m_hl != 0.0) {
            let luma_mh = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
            let hl_w = smoothstep(0.5, 1.5, luma_mh);
            let strength = select(0.4, 0.6, m_hl < 0.0);
            color = color * max(1.0 + m_hl * strength * hl_w, 0.2);
        }

        // Shadows: same additive luma-weighted lift as Step 10
        let m_sh = m.adj1.y * w;
        if (m_sh != 0.0) {
            let luma_ms = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
            let sh_w = clamp(1.0 - luma_ms / 0.5, 0.0, 1.0);
            color = color + vec3<f32>(sh_w * m_sh * 0.15);
        }

        // Contrast: pivoted at middle grey, matching Step 11's form
        color = (color - 0.18) * max(1.0 + m.adj0.y * w, 0.0) + 0.18;

        // Saturation: luma-preserving mix, matching Step 13's form
        let luma_msat = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
        color = mix(vec3<f32>(luma_msat), color, 1.0 + m.adj0.z * w);

        color = max(color, vec3<f32>(0.0));
    }

    // ── Step 11: Contrast ─────────────────────────────────────────────────────
    let contrast_factor = 1.0 + (params.contrast / 100.0);
    color = (color - 0.18) * contrast_factor + 0.18;

    // ── Step 12: Levels (Whites & Blacks) ────────────────────────────────────
    color = (color - vec3<f32>(params.blacks)) / vec3<f32>(params.whites - params.blacks + 0.0001);

    // ── Step 13: Saturation ───────────────────────────────────────────────────
    var luma = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let sat_factor = 1.0 + (params.saturation / 100.0);
    color = mix(vec3<f32>(luma), color, sat_factor);

    // ── Step 14: Vibrance ─────────────────────────────────────────────────────
    let chroma = max(color.r, max(color.g, color.b)) - min(color.r, min(color.g, color.b));
    let vibrance_amount = params.vibrance * (1.0 - chroma);
    luma = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = mix(vec3<f32>(luma), color, 1.0 + vibrance_amount);

    // ── Step 15: Filmic Tone Curve (S-curve with highlight protection) ──────────
    // Approximates the camera picture-style curve visible in the embedded JPEG:
    //
    //  1. Midtone lift — f(x) = x·(1 + t − t·x), t = 0.08.
    //     Lifts 18 % grey by ≈ 6 %, fades to 0 at black and white.
    //     Gives the slight "pop" that camera Standard picture styles apply.
    //
    //  2. Highlight shoulder — Reinhard rolloff starting at 0.65 linear (≈ 85 % sRGB).
    //     Old threshold was 0.80 (≈ 91 % sRGB), leaving only 20 % headroom and
    //     blowing out cloud / sky detail.  0.65 gives 35 % headroom and matches
    //     the typical camera highlight-protection curve.
    // When the active DCP embeds a ProfileToneCurve (applied in the DCP block
    // above), it already provides the rendering S-curve — running the filmic
    // lift + shoulder on top double-tone-maps: lifted mids on lifted mids and a
    // second shoulder crushing highlights the profile curve already rolled off.
    // "Adobe Standard" DCPs carry NO embedded curve (they expect the host's
    // default curve), so the filmic curve stays on for them and for the
    // no-DCP fallback path.
    //
    // Tone rendering is ONE selected stage (params.tone_mapper). Reaching it
    // splits by path, because the two paths tone-map at different points:
    //   • DCP path — already handled above, at the ProfileToneCurve slot,
    //     which is where a tone curve belongs relative to the HueSatMap.
    //   • Fallback path (no profile) — nothing has curved it yet, so the
    //     selected operator is applied here.
    // Getting this gate wrong double-tone-maps, which is what the old
    // dcp_has_curve condition existed to prevent.
    color = max(color, vec3<f32>(0.0));
    if (params.tone_mapper != TONE_CAMERA) {
        if (params.has_dcp == 0u) {
            // Same baked LUT and same hue-preserving application the DCP path
            // uses — the operator does not care which colour space fed it, and
            // RGBTone beats the naive per-channel application these operators
            // normally get.
            color = dcp_rgb_tone(color);
        }
    } else if (params.has_dcp == 0u || params.dcp_has_curve == 0u) {
        // Camera operator with no curve to speak of: the built-in filmic
        // rendering. Mirrored on the CPU as core::tonemap::filmic, so
        // selecting "Filmic" explicitly produces identical numbers.
        let tone_lift: f32 = 0.08;
        color = color * (vec3<f32>(1.0 + tone_lift) - tone_lift * color);

        let pre_tone_max = max(color.r, max(color.g, color.b));
        if (pre_tone_max > 0.0) {
            let threshold: f32 = 0.65;
            let headroom:  f32 = 1.0 - threshold; // 0.35
            var post_tone_max: f32;
            if (pre_tone_max <= threshold) {
                post_tone_max = pre_tone_max;
            } else {
                let over_t = pre_tone_max - threshold;
                post_tone_max = threshold + (headroom * over_t) / (over_t + headroom);
            }
            color = color * (post_tone_max / pre_tone_max);
        }
    }

    // ── Step 15b: Fit out-of-gamut overbrights (luma-preserving) ────────────
    // Saturated colours can exceed 1.0 in one channel after the DCP pipeline
    // (e.g. a bright red: sRGB (1.3, 0.2, 0.15)).  The final clamp would crush
    // that to the gamut boundary at max saturation — neon red.  Compress chroma
    // toward luma just enough to bring the peak channel to 1.0 instead.
    // Runs after the filmic shoulder so the fallback path's rolloff still sees
    // >1.0 input; for near-white pixels (luma ≥ 1) scale uniformly.
    // Two candidate mappings, blended by the pixel's own saturation:
    //  • scale-by-max  — keeps hue AND saturation, gives up brightness.
    //    Right for saturated colours (a bright red must stay red, darker).
    //  • luma-preserving desat — keeps brightness, gives up chroma.
    //    Right for near-neutral overbrights (blown sky should go white).
    let fit_max = max(color.r, max(color.g, color.b));
    if (fit_max > 1.0) {
        let fit_min  = min(color.r, min(color.g, color.b));
        let fit_luma = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
        let fit_sat  = clamp((fit_max - fit_min) / fit_max, 0.0, 1.0);
        let scaled = color / fit_max;
        var desat: vec3<f32>;
        if (fit_luma < 1.0) {
            let t_fit = (1.0 - fit_luma) / (fit_max - fit_luma);
            desat = vec3<f32>(fit_luma) + (color - vec3<f32>(fit_luma)) * t_fit;
        } else {
            desat = scaled;
        }
        color = mix(desat, scaled, fit_sat);
    }

    // ── Step 15: sRGB Transfer Function (IEC 61966-2-1) ──────────────────────
    color = vec3<f32>(
        linear_to_srgb(color.r),
        linear_to_srgb(color.g),
        linear_to_srgb(color.b)
    );

    // ── Step 16: Clamp to display range ──────────────────────────────────────
    color = clamp(color, vec3<f32>(0.0), vec3<f32>(1.0));

    // ── Step 17: Point curves (display-encoded, like every curve tool) ──────
    // Per channel: the LUT already composes each channel's curve after the
    // master, so this is three fetches, not six.
    if (params.curve_active == 1u) {
        color = vec3<f32>(sample_curve(color.r).r, sample_curve(color.g).g, sample_curve(color.b).b);
    }

    // Mask overlay: show the selected mask's coverage while editing it.
    if (params.mask_overlay > 0u) {
        color = mix(color, vec3<f32>(1.0, 0.18, 0.12), overlay_w * 0.55);
    }

    return vec4<f32>(color, 1.0);
}
"#;

/// Get the color science shader (Pass 2 — reads debayered Rgba16Float)
pub fn get_color_shader() -> &'static str {
    PASSTHROUGH_SHADER
}


#[cfg(test)]
mod tests {
    use super::*;

    fn validate_wgsl(source: &str, name: &str) {
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|e| panic!("{name} failed to parse: {}", e.emit_to_string(source)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name} failed validation: {e:?}"));
    }

    #[test]
    fn color_shader_is_valid_wgsl() {
        validate_wgsl(PASSTHROUGH_SHADER, "PASSTHROUGH_SHADER");
    }

    #[test]
    fn shader_mask_constants_match_rust() {
        let expected = format!(
            "const MASK_CHROMA_EPS: f32 = {};",
            crate::core::types::MASK_CHROMA_EPS
        );
        assert!(PASSTHROUGH_SHADER.contains(&expected), "missing `{expected}`");
    }

    /// The tone LUT's domain is encoded on the CPU (at bake time) and decoded
    /// in the shader (at sample time). If the two constants drift, every
    /// lookup lands at the wrong scene-linear value and the whole image
    /// shifts — silently, with no crash and no validation error.
    #[test]
    fn shader_tone_lut_constants_match_rust() {
        let expected = format!(
            "const TONE_LUT_MAX: f32 = {:.1};",
            crate::core::tonemap::TONE_LUT_MAX
        );
        assert!(
            PASSTHROUGH_SHADER.contains(&expected),
            "shader is missing `{expected}` — CPU/GPU tone LUT domains disagree"
        );
    }

}
