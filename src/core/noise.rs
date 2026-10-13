//! Per-image sensor noise model, estimated from the mosaic itself.
//!
//! Raw sensor noise is well described as Poisson-Gaussian: a pixel recording
//! signal `x` has variance `a·x + b` — shot noise grows with the signal, read
//! noise is a floor. `a` and `b` change with ISO, sensor and (for previews)
//! with how much the mosaic was box-filtered, so a fixed denoise threshold is
//! too strong on a base-ISO frame and too weak at ISO 3200. Knowing the model
//! lets the denoiser work in a **variance-stabilised** space (generalised
//! Anscombe transform) where every pixel's noise is ≈ 1, so its thresholds
//! are expressed in units of the actual noise and one slider position means
//! the same thing on every image.
//!
//! Estimation: at each sampled site, the second difference of same-colour
//! neighbours two pixels away, `v(x-2) − 2·v(x) + v(x+2)`, cancels any linear
//! gradient and leaves (mostly) noise with variance `6·σ²(x)`. Sites are binned
//! by their local mean; a robust (MAD-based) variance per bin rejects the
//! edges and texture that survive the differencing; a weighted least-squares
//! line through the bins gives `a` and `b`.

/// `variance(x) = a·x + b` per colour (R, G, B), in white-balanced unit-range
/// units — the same units the demosaic writes before un-balancing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseModel {
    pub a: [f32; 3],
    pub b: [f32; 3],
}

impl Default for NoiseModel {
    /// A plausible low-ISO model, used when estimation has too little data.
    fn default() -> Self {
        Self { a: [6e-5; 3], b: [2e-7; 3] }
    }
}

impl NoiseModel {
    /// Coefficients of the generalised Anscombe transform for colour `c`:
    /// `s(x) = k · sqrt(max(a·x + c0, 0))`, giving unit noise variance.
    /// Returned as `(a, c0, k)` for the shader.
    pub fn vst(&self, c: usize) -> (f32, f32, f32) {
        let a = self.a[c].max(1e-7);
        let b = self.b[c].max(0.0);
        (a, 0.375 * a * a + b, 2.0 / a)
    }
}

const BINS: usize = 24;
/// Samples per bin below which the bin's variance is not trusted.
const MIN_BIN_SAMPLES: usize = 64;

/// Estimate the noise model of a Bayer mosaic.
///
/// `colors`, `black` and `scale` are per 2×2 tile position
/// (`k = (y & 1) * 2 + (x & 1)`), exactly as `gpu::demosaic` uses them, so the
/// model describes the white-balanced values the denoiser will see. `ceil` is
/// each colour's full-scale white-balanced value (its WB multiplier).
pub fn estimate(
    mosaic: &[u16],
    width: usize,
    height: usize,
    colors: [u32; 4],
    black: [f32; 4],
    scale: [f32; 4],
    ceil: [f32; 3],
) -> NoiseModel {
    if width < 16 || height < 16 || mosaic.len() < width * height {
        return NoiseModel::default();
    }
    let value = |x: usize, y: usize| -> f32 {
        let k = (y & 1) * 2 + (x & 1);
        (mosaic[y * width + x] as f32 - black[k]).max(0.0) * scale[k]
    };

    // Sample on a sparse grid: a few hundred thousand differences are plenty,
    // and this must stay cheap at 45 MP. Steps are odd so every tile position
    // (and so every colour) is visited.
    let target = 400_000usize;
    let step = ((width * height / target) as f64).sqrt().floor().max(1.0) as usize;
    let step = if step.is_multiple_of(2) { step + 1 } else { step };

    // Per colour, per bin: |second differences| and the sum of local means.
    let mut diffs: Vec<Vec<Vec<f32>>> = vec![vec![Vec::new(); BINS]; 3];
    let mut means: Vec<Vec<f64>> = vec![vec![0.0; BINS]; 3];

    let mut y = 2;
    while y < height - 2 {
        let mut x = 2;
        while x < width - 2 {
            let k = (y & 1) * 2 + (x & 1);
            let c = colors[k] as usize;
            let v0 = value(x, y);
            for (l, r) in [(value(x - 2, y), value(x + 2, y)), (value(x, y - 2), value(x, y + 2))] {
                let m = (l + v0 + r) / 3.0;
                // Bins span this colour's white-balanced range; the top 10%
                // is clipping-compressed and would bend the fit.
                let range = ceil[c].max(1e-6);
                if m <= 0.0 || m >= range * 0.9 {
                    continue;
                }
                let bin = ((m / range) * BINS as f32) as usize;
                let bin = bin.min(BINS - 1);
                diffs[c][bin].push((l - 2.0 * v0 + r).abs());
                means[c][bin] += m as f64;
            }
            x += step;
        }
        y += step;
    }

    let mut model = NoiseModel::default();
    for c in 0..3 {
        // (mean, variance, weight) per usable bin.
        let mut pts: Vec<(f64, f64, f64)> = Vec::new();
        for bin in 0..BINS {
            let d = &mut diffs[c][bin];
            if d.len() < MIN_BIN_SAMPLES {
                continue;
            }
            let n = d.len();
            let mid = n / 2;
            let (_, median, _) = d.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
            // |N(0, s)| has median 0.6745·s; the second difference has
            // variance 6σ².
            let s = *median as f64 / 0.6745;
            let var = s * s / 6.0;
            pts.push((means[c][bin] / n as f64, var, n as f64));
        }
        if let Some((a, b)) = fit_affine(&pts) {
            model.a[c] = a as f32;
            model.b[c] = b as f32;
        }
    }
    model
}

/// Weighted least squares for `var = a·mean + b`, with `a ≥ 0`, `b ≥ 0`.
///
/// Bins high in texture inflate their variance; weighting by sample count
/// and using the median per bin keeps them from dominating. Returns `None`
/// with fewer than two usable bins.
fn fit_affine(pts: &[(f64, f64, f64)]) -> Option<(f64, f64)> {
    if pts.len() < 2 {
        return None;
    }
    let sw: f64 = pts.iter().map(|p| p.2).sum();
    let mx = pts.iter().map(|p| p.0 * p.2).sum::<f64>() / sw;
    let my = pts.iter().map(|p| p.1 * p.2).sum::<f64>() / sw;
    let sxx: f64 = pts.iter().map(|p| p.2 * (p.0 - mx).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| p.2 * (p.0 - mx) * (p.1 - my)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let mut a = sxy / sxx;
    let mut b = my - a * mx;
    if a < 0.0 {
        // Pure read noise (or a broken fit): flat variance.
        a = 1e-7;
        b = my;
    }
    if b < 0.0 {
        // Read noise below what the fit can resolve: refit through origin.
        let sxy0: f64 = pts.iter().map(|p| p.2 * p.0 * p.1).sum();
        let sxx0: f64 = pts.iter().map(|p| p.2 * p.0 * p.0).sum();
        a = (sxy0 / sxx0).max(1e-7);
        b = 0.0;
    }
    Some((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64* — deterministic, dependency-free noise for the tests.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u1 = self.next_f64().max(1e-12);
            let u2 = self.next_f64();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// Build an RGGB mosaic of smooth content with known Poisson-Gaussian
    /// noise, then check the estimate recovers the model.
    #[test]
    fn recovers_a_known_noise_model() {
        let (w, h) = (1200usize, 800usize);
        let (white, blk) = (16383.0f64, 600.0f64);
        // In normalised (unit-range, unbalanced) units.
        let (a_true, b_true) = (4e-4f64, 4e-6f64);
        let wb = [2.0f32, 1.0, 1.5];
        let colors = [0u32, 1, 1, 2];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut mosaic = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                // Smooth ramps spanning most of the range, different per axis.
                let signal = 0.03 + 0.85 * ((x as f64 / w as f64) * 0.7 + (y as f64 / h as f64) * 0.3);
                let sd = (a_true * signal + b_true).sqrt();
                let v = (signal + sd * rng.gauss()).clamp(0.0, 1.0);
                mosaic[y * w + x] = (blk + v * (white - blk)).round() as u16;
            }
        }
        let black = [blk as f32; 4];
        let mut scale = [0.0f32; 4];
        for k in 0..4 {
            scale[k] = wb[colors[k] as usize] / (white as f32 - blk as f32);
        }
        let m = estimate(&mosaic, w, h, colors, black, scale, wb);
        // White balance multiplies values by wb: variance scales by wb²,
        // so in balanced units a_c = wb·a and b_c = wb²·b.
        for (c, &wbc) in wb.iter().enumerate() {
            let wbc = wbc as f64;
            let a_exp = a_true * wbc;
            let b_exp = b_true * wbc * wbc;
            let a_err = (m.a[c] as f64 - a_exp).abs() / a_exp;
            eprintln!("c{c}: a {:.3e} (expect {:.3e}), b {:.3e} (expect {:.3e})", m.a[c], a_exp, m.b[c], b_exp);
            assert!(a_err < 0.15, "colour {c}: a off by {:.0}%", a_err * 100.0);
            // b is small next to a·x over most of the range; only require the
            // right order of magnitude.
            assert!(m.b[c] as f64 <= b_exp * 4.0 + 2e-6, "colour {c}: b far too large");
        }
    }

    /// The transform must actually stabilise: noise of unit variance at both
    /// ends of the range.
    #[test]
    fn vst_gives_unit_noise_across_the_range() {
        let model = NoiseModel { a: [4e-4; 3], b: [4e-6; 3] };
        let (a, c0, k) = model.vst(1);
        let s = |x: f64| k as f64 * (a as f64 * x + c0 as f64).max(0.0).sqrt();
        for x in [0.01f64, 0.1, 0.5, 0.9] {
            let sd = (4e-4 * x + 4e-6).sqrt();
            let slope = (s(x + 1e-6) - s(x - 1e-6)) / 2e-6;
            let sd_s = slope * sd;
            assert!((sd_s - 1.0).abs() < 0.1, "x={x}: stabilised σ {sd_s:.3}");
        }
    }

    #[test]
    fn tiny_inputs_fall_back_to_the_default() {
        let m = estimate(&[0u16; 64], 8, 8, [0, 1, 1, 2], [0.0; 4], [1.0; 4], [1.0; 3]);
        assert_eq!(m, NoiseModel::default());
    }
}
