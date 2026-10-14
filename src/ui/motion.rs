//! Motion: the timing primitives behind the UI's animations.
//!
//! Three tools, each for a different job:
//!
//! - **Springs** ([`Spring`], [`Animated`]) for anything that *moves* — the tab
//!   indicator, a button's press depth, a scroll glide. A spring carries
//!   velocity, so retargeting mid-flight bends the path instead of restarting
//!   it, and slight underdamping is what gives motion its sense of weight:
//!   things arrive, overshoot a hair, and settle.
//! - **Timelines** ([`Timeline`] + a [`CubicBezier`]) for anything that
//!   *appears* — fades and entrances. Fixed durations keep choreography (the
//!   modal surface first, then its content) in step.
//! - **Exponential smoothing** ([`approach_factor`]) for colour: hover and
//!   selection fades. Frame-rate independent and seamless to retarget.
//!
//! Everything here is plain arithmetic on `f32`s and `Instant`s so it can be
//! unit tested; the widgets in `ui::widgets` are thin shells around it.
//!
//! Widgets animate *themselves*: they keep this state in their iced widget
//! tree, advance it on `window::Event::RedrawRequested`, and request the next
//! frame only while something is still moving. No `Message`, no `update`, no
//! `view` rebuild per frame — which matters, because iced rebuilds the whole
//! view after every message (see CLAUDE.md §E).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use iced::{Background, Border, Color, Shadow, Vector};

// ── Global switches ─────────────────────────────────────────────────────────

static REDUCED: AtomicBool = AtomicBool::new(false);

/// Preferences › Reduce motion. Global rather than threaded through every
/// view: widgets have no access to editor state, and this is a process-wide
/// preference by nature. With it on, every animation lands on its final frame
/// immediately.
pub fn set_reduced(on: bool) {
    REDUCED.store(on, Ordering::Relaxed);
}

pub fn reduced() -> bool {
    REDUCED.load(Ordering::Relaxed)
}

/// Developer aid: `RAW_EDITOR_SLOW_MOTION=10` plays every animation ten times
/// slower, for tuning curves by eye (the equivalent of Android's "animator
/// duration scale"). Read once.
fn time_scale() -> f32 {
    static SCALE: LazyLock<f32> = LazyLock::new(|| {
        std::env::var("RAW_EDITOR_SLOW_MOTION")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.clamp(0.1, 100.0))
            .unwrap_or(1.0)
    });
    *SCALE
}

/// Longest step a single frame may advance an animation by. A frame that
/// stalls (a RAW decode landing, the window being dragged) must not teleport
/// a spring past its target or skip a fade.
const MAX_FRAME_DT: f32 = 1.0 / 20.0;

/// Seconds of *animation time* between two instants.
pub fn elapsed(from: Instant, to: Instant) -> f32 {
    to.saturating_duration_since(from).as_secs_f32() / time_scale()
}

/// Animation time since the previous frame, `0` on the first frame of a run
/// (so an animation that starts after a long idle doesn't jump straight to
/// its end), clamped to [`MAX_FRAME_DT`].
pub fn frame_dt(last: Option<Instant>, now: Instant) -> f32 {
    last.map_or(0.0, |last| elapsed(last, now).min(MAX_FRAME_DT))
}

/// Wall-clock length of an animation of `secs` — for timers that have to
/// outlive one (the modal exit). Zero under reduced motion.
pub fn wall_duration(secs: f32) -> Duration {
    if reduced() {
        Duration::ZERO
    } else {
        Duration::from_secs_f32(secs * time_scale())
    }
}

// ── Durations ───────────────────────────────────────────────────────────────

/// The house timings, in seconds. Fast enough never to sit between the user
/// and their work; long enough for the eye to read the change as motion.
pub mod duration {
    /// Thumbnail fades / crossfades.
    pub const FADE: f32 = 0.22;
    /// Tab content fade-through.
    pub const PAGE: f32 = 0.26;
    /// Modal entrance (surface + content, staggered within it).
    pub const MODAL_ENTER: f32 = 0.30;
    /// Modal exit. Exits are quicker than entrances: the user has already
    /// decided, so the UI gets out of the way.
    pub const MODAL_EXIT: f32 = 0.16;
}

// ── Easing curves ───────────────────────────────────────────────────────────

/// A CSS-style `cubic-bezier(x1, y1, x2, y2)` timing function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Eased value at linear progress `t` (clamped to `[0, 1]`).
    pub fn ease(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t == 0.0 || t == 1.0 {
            return t;
        }
        Self::sample(self.y1, self.y2, self.solve_x(t))
    }

    /// One coordinate of the curve at parameter `s` (endpoints 0 and 1).
    fn sample(p1: f32, p2: f32, s: f32) -> f32 {
        let inv = 1.0 - s;
        3.0 * inv * inv * s * p1 + 3.0 * inv * s * s * p2 + s * s * s
    }

    fn slope(p1: f32, p2: f32, s: f32) -> f32 {
        let inv = 1.0 - s;
        3.0 * inv * inv * p1 + 6.0 * inv * s * (p2 - p1) + 3.0 * s * s * (1.0 - p2)
    }

    /// The curve parameter whose x is `x`: Newton first (fast, almost always
    /// converges), bisection when the slope flattens out.
    fn solve_x(&self, x: f32) -> f32 {
        let mut s = x;
        for _ in 0..8 {
            let err = Self::sample(self.x1, self.x2, s) - x;
            if err.abs() < 1e-6 {
                return s;
            }
            let d = Self::slope(self.x1, self.x2, s);
            if d.abs() < 1e-6 {
                break;
            }
            s = (s - err / d).clamp(0.0, 1.0);
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        s = x;
        for _ in 0..32 {
            let v = Self::sample(self.x1, self.x2, s);
            if (v - x).abs() < 1e-6 {
                break;
            }
            if v < x {
                lo = s;
            } else {
                hi = s;
            }
            s = 0.5 * (lo + hi);
        }
        s
    }
}

pub mod ease {
    use super::CubicBezier;

    /// Arrives fast and spends its time settling: the curve for anything
    /// entering the screen. It front-loads the motion so the UI responds
    /// instantly, then lets the last few pixels land gently.
    pub const EMPHASIZED: CubicBezier = CubicBezier::new(0.05, 0.7, 0.1, 1.0);
    /// General-purpose decelerate, for fades.
    pub const STANDARD: CubicBezier = CubicBezier::new(0.2, 0.0, 0.0, 1.0);
    /// Leaves gently and accelerates away: the curve for exits.
    pub const EXIT: CubicBezier = CubicBezier::new(0.3, 0.0, 1.0, 1.0);
}

// ── Springs ─────────────────────────────────────────────────────────────────

/// A damped spring, parameterised the way designers think about one:
///
/// - `response`: roughly how long (seconds) it takes to get there — the
///   period of the undamped oscillation.
/// - `damping`: the damping ratio. `1.0` arrives without overshoot; lower
///   values overshoot and settle, which reads as momentum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    pub response: f32,
    pub damping: f32,
}

impl Spring {
    /// Quick, a touch of overshoot. Indicators and things that slide.
    pub const SNAPPY: Spring = Spring { response: 0.30, damping: 0.78 };
    /// A heavier, unhurried follow — the trailing edge of a stretching
    /// indicator.
    pub const TRAIL: Spring = Spring { response: 0.42, damping: 0.86 };
    /// No overshoot. For things that must not exceed their target: heights,
    /// scroll offsets.
    pub const SMOOTH: Spring = Spring { response: 0.34, damping: 1.0 };
    /// A button going down: immediate and dead.
    pub const PRESS: Spring = Spring { response: 0.10, damping: 1.0 };
    /// A button coming back up: springy, so a tap lands with a little bounce.
    pub const RELEASE: Spring = Spring { response: 0.32, damping: 0.55 };
    /// Pop-in for small badges: lively, noticeable overshoot.
    pub const POP: Spring = Spring { response: 0.34, damping: 0.62 };

    /// Advance `(value, velocity)` towards `target` by `dt` seconds.
    ///
    /// Semi-implicit Euler in fixed substeps: stable for any spring here at
    /// any frame rate, and far cheaper to reason about than the closed form.
    pub fn step(&self, value: &mut f32, velocity: &mut f32, target: f32, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let omega = std::f32::consts::TAU / self.response.max(1e-3);
        let stiffness = omega * omega;
        let damping = 2.0 * self.damping * omega;
        let steps = (dt / 0.002).ceil().clamp(1.0, 64.0) as usize;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = -stiffness * (*value - target) - damping * *velocity;
            *velocity += accel * h;
            *value += *velocity * h;
        }
    }
}

/// A scalar driven by a spring towards a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animated {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
}

/// Distance / speed below which a spring counts as at rest. Values animated
/// here are fractions or segment units, where 1e-3 is far below a pixel.
const REST_DISTANCE: f32 = 1e-3;
const REST_SPEED: f32 = 1e-2;

impl Animated {
    pub fn new(value: f32) -> Self {
        Self { value, velocity: 0.0, target: value }
    }

    /// Head for `target`, keeping the current velocity.
    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump to `value` and stop.
    pub fn snap(&mut self, value: f32) {
        *self = Self::new(value);
    }

    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() < REST_DISTANCE && self.velocity.abs() < REST_SPEED
    }

    /// Advance by `dt`; returns `true` while still moving. Lands exactly on
    /// the target once at rest, so callers can compare against it.
    pub fn step(&mut self, spring: Spring, dt: f32) -> bool {
        if reduced() {
            self.snap(self.target);
            return false;
        }
        spring.step(&mut self.value, &mut self.velocity, self.target, dt);
        if self.is_settled() {
            self.snap(self.target);
            false
        } else {
            true
        }
    }
}

// ── Timelines ───────────────────────────────────────────────────────────────

/// A fixed-duration clock that starts on the first frame it is *shown*, not
/// when it was built.
///
/// A widget's state is created during the view rebuild, some time before its
/// first frame is drawn; starting the clock there would skip the opening
/// frames of an entrance. So a timeline is *armed* first and started by the
/// next `RedrawRequested` the widget sees.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Timeline {
    /// Finished (or never run): progress 1.
    #[default]
    Idle,
    /// Will start on the next frame: progress 0.
    Armed,
    Running(Instant),
}

impl Timeline {
    /// Arm unless reduced motion is on, in which case go straight to the end.
    pub fn armed() -> Self {
        if reduced() {
            Self::Idle
        } else {
            Self::Armed
        }
    }

    /// Call on every `RedrawRequested`: starts an armed timeline and retires
    /// a finished one. Returns `true` while it still needs frames.
    pub fn tick(&mut self, now: Instant, duration: f32) -> bool {
        match *self {
            Self::Idle => false,
            Self::Armed => {
                *self = Self::Running(now);
                true
            }
            Self::Running(start) => {
                if elapsed(start, now) >= duration {
                    *self = Self::Idle;
                    false
                } else {
                    true
                }
            }
        }
    }

    /// Linear progress in `[0, 1]` at `now` (`None`: no frame seen yet).
    pub fn progress(&self, now: Option<Instant>, duration: f32) -> f32 {
        match (*self, now) {
            (Self::Idle, _) => 1.0,
            (Self::Armed, _) | (Self::Running(_), None) => 0.0,
            (Self::Running(start), Some(now)) => {
                (elapsed(start, now) / duration.max(1e-3)).clamp(0.0, 1.0)
            }
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// Re-map `t` so that `[start, end]` becomes `[0, 1]` (clamped) — for
/// staggering several things within one timeline.
pub fn segment(t: f32, start: f32, end: f32) -> f32 {
    ((t - start) / (end - start).max(1e-6)).clamp(0.0, 1.0)
}

// ── Smoothing and mixing ────────────────────────────────────────────────────

/// Fraction of the remaining distance to cover this frame, for exponential
/// smoothing with time constant `tau` (seconds): ~63% of the way after `tau`,
/// ~95% after `3·tau`, regardless of frame rate.
pub fn approach_factor(dt: f32, tau: f32) -> f32 {
    if reduced() || tau <= 0.0 {
        1.0
    } else {
        1.0 - (-dt / tau).exp()
    }
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
        a: lerp(a.a, b.a, t),
    }
}

/// Mix two colours where either may be "absent" (fully transparent): an
/// absent end borrows the other end's RGB, so a fade from nothing to grey is
/// a pure alpha fade rather than a detour through transparent *black*.
pub fn mix_color_opt(a: Option<Color>, b: Option<Color>, t: f32) -> Option<Color> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(lerp_color(a, Color { a: 0.0, ..a }, t)),
        (None, Some(b)) => Some(lerp_color(Color { a: 0.0, ..b }, b, t)),
        (Some(a), Some(b)) => Some(lerp_color(a, b, t)),
    }
}

/// Mix backgrounds. Gradients can't be meaningfully interpolated here, so a
/// gradient on either side switches over at the halfway point.
pub fn mix_background(a: Option<Background>, b: Option<Background>, t: f32) -> Option<Background> {
    match (a, b) {
        (Some(Background::Gradient(_)), _) | (_, Some(Background::Gradient(_))) => {
            if t < 0.5 {
                a
            } else {
                b
            }
        }
        _ => {
            let color = |bg: Option<Background>| match bg {
                Some(Background::Color(c)) => Some(c),
                _ => None,
            };
            mix_color_opt(color(a), color(b), t).map(Background::Color)
        }
    }
}

/// A border's colour only shows where it has width, so a zero-width border is
/// treated as transparent when mixing (otherwise a border fading in would
/// flash whatever colour the borderless style happened to declare).
pub fn mix_border(a: Border, b: Border, t: f32) -> Border {
    let visible = |border: Border| (border.width > 0.0).then_some(border.color);
    Border {
        color: mix_color_opt(visible(a), visible(b), t).unwrap_or(Color::TRANSPARENT),
        width: lerp(a.width, b.width, t),
        radius: iced::border::Radius {
            top_left: lerp(a.radius.top_left, b.radius.top_left, t),
            top_right: lerp(a.radius.top_right, b.radius.top_right, t),
            bottom_right: lerp(a.radius.bottom_right, b.radius.bottom_right, t),
            bottom_left: lerp(a.radius.bottom_left, b.radius.bottom_left, t),
        },
    }
}

pub fn mix_shadow(a: Shadow, b: Shadow, t: f32) -> Shadow {
    let visible = |s: Shadow| (s.color.a > 0.0).then_some(s.color);
    Shadow {
        color: mix_color_opt(visible(a), visible(b), t).unwrap_or(Color::TRANSPARENT),
        offset: Vector::new(lerp(a.offset.x, b.offset.x, t), lerp(a.offset.y, b.offset.y, t)),
        blur_radius: lerp(a.blur_radius, b.blur_radius, t),
    }
}

/// Largest per-channel difference between two colours.
pub fn color_distance(a: Color, b: Color) -> f32 {
    (a.r - b.r)
        .abs()
        .max((a.g - b.g).abs())
        .max((a.b - b.b).abs())
        .max((a.a - b.a).abs())
}

/// `color` with its alpha multiplied by `alpha`.
pub fn fade(color: Color, alpha: f32) -> Color {
    Color { a: color.a * alpha.clamp(0.0, 1.0), ..color }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bezier_hits_its_endpoints_and_is_monotonic() {
        for curve in [ease::EMPHASIZED, ease::STANDARD, ease::EXIT] {
            assert_eq!(curve.ease(0.0), 0.0);
            assert_eq!(curve.ease(1.0), 1.0);
            let mut prev = 0.0;
            for i in 1..=100 {
                let v = curve.ease(i as f32 / 100.0);
                assert!(v >= prev - 1e-4, "{curve:?} not monotonic at {i}: {prev} -> {v}");
                prev = v;
            }
        }
    }

    #[test]
    fn bezier_matches_known_values() {
        // The linear curve is the identity.
        let linear = CubicBezier::new(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((linear.ease(t) - t).abs() < 1e-4);
        }
        // CSS `ease-out` (0, 0, .58, 1) at x = 0.5 is ≈ 0.68464 (by bisection).
        let ease_out = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
        assert!((ease_out.ease(0.5) - 0.68464).abs() < 1e-3, "{}", ease_out.ease(0.5));
    }

    /// The entrance curve must front-load: most of the motion in the first
    /// third, which is what makes a 300ms entrance feel instant.
    #[test]
    fn emphasized_curve_front_loads_the_motion() {
        assert!(ease::EMPHASIZED.ease(0.3) > 0.8);
        assert!(ease::EXIT.ease(0.3) < 0.2, "exits start gently");
    }

    fn settle(spring: Spring, from: f32, to: f32) -> (f32, f32, usize) {
        let mut a = Animated::new(from);
        a.set(to);
        let (mut peak, mut frames) = (from, 0);
        while a.step(spring, 1.0 / 60.0) {
            peak = if to > from { peak.max(a.value) } else { peak.min(a.value) };
            frames += 1;
            assert!(frames < 600, "{spring:?} never settled");
        }
        (a.value, peak, frames)
    }

    #[test]
    fn springs_settle_exactly_on_target() {
        for spring in [
            Spring::SNAPPY,
            Spring::TRAIL,
            Spring::SMOOTH,
            Spring::PRESS,
            Spring::RELEASE,
            Spring::POP,
        ] {
            let (value, _, frames) = settle(spring, 0.0, 1.0);
            assert_eq!(value, 1.0, "{spring:?}");
            // Fast enough to never be in the way: under a second at 60fps.
            assert!(frames < 60, "{spring:?} took {frames} frames");
        }
    }

    #[test]
    fn critically_damped_springs_never_overshoot() {
        for spring in [Spring::SMOOTH, Spring::PRESS] {
            let (_, peak, _) = settle(spring, 0.0, 1.0);
            assert!(peak <= 1.0 + 1e-4, "{spring:?} overshot to {peak}");
        }
    }

    /// The overshoot is the "weight": present, but a hair, not a wobble.
    #[test]
    fn underdamped_springs_overshoot_a_little() {
        let (_, peak, _) = settle(Spring::SNAPPY, 0.0, 1.0);
        assert!(peak > 1.005 && peak < 1.1, "SNAPPY peak {peak}");
        let (_, peak, _) = settle(Spring::RELEASE, 0.0, 1.0);
        assert!(peak > 1.05 && peak < 1.25, "RELEASE peak {peak}");
    }

    /// Frame-rate independence: the same second of animation lands in the
    /// same place at 30fps and 144fps.
    #[test]
    fn springs_are_frame_rate_independent() {
        let run = |fps: f32| {
            let mut a = Animated::new(0.0);
            a.set(1.0);
            // 1/6 s is a whole number of frames at both rates.
            let frames = (fps / 6.0).round() as usize;
            for _ in 0..frames {
                a.step(Spring::SNAPPY, 1.0 / fps);
            }
            a.value
        };
        assert!((run(30.0) - run(144.0)).abs() < 0.02);
    }

    /// Retargeting keeps velocity: the value keeps moving the way it was
    /// going for a moment instead of reversing instantly.
    #[test]
    fn retargeting_a_spring_keeps_its_momentum() {
        let mut a = Animated::new(0.0);
        a.set(1.0);
        for _ in 0..4 {
            a.step(Spring::SNAPPY, 1.0 / 60.0);
        }
        let v = a.value;
        assert!(a.velocity > 0.0);
        a.set(0.0);
        a.step(Spring::SNAPPY, 1.0 / 60.0);
        assert!(a.value > v, "reversed instantly: {v} -> {}", a.value);
    }

    #[test]
    fn timeline_starts_on_its_first_frame() {
        let t0 = Instant::now();
        let mut tl = Timeline::Armed;
        assert_eq!(tl.progress(None, 0.2), 0.0);
        // The first frame starts it at zero, however late it comes.
        assert!(tl.tick(t0, 0.2));
        assert_eq!(tl.progress(Some(t0), 0.2), 0.0);
        let mid = t0 + Duration::from_millis(100);
        assert!(tl.tick(mid, 0.2));
        assert!((tl.progress(Some(mid), 0.2) - 0.5).abs() < 0.05);
        let end = t0 + Duration::from_millis(250);
        assert!(!tl.tick(end, 0.2));
        assert!(tl.is_idle());
        assert_eq!(tl.progress(Some(end), 0.2), 1.0);
    }

    #[test]
    fn segment_staggers_within_a_timeline() {
        assert_eq!(segment(0.1, 0.3, 0.9), 0.0);
        assert!((segment(0.6, 0.3, 0.9) - 0.5).abs() < 1e-6);
        assert_eq!(segment(1.0, 0.3, 0.9), 1.0);
    }

    #[test]
    fn approach_is_frame_rate_independent() {
        let run = |fps: f32| {
            let mut v = 0.0;
            for _ in 0..(0.1 * fps) as usize {
                v = lerp(v, 1.0, approach_factor(1.0 / fps, 0.05));
            }
            v
        };
        assert!((run(30.0) - run(240.0)).abs() < 0.01);
        // ~1 - e^-2 after two time constants.
        assert!((run(60.0) - 0.8647).abs() < 0.01);
    }

    /// Fading a background in from nothing must not pass through black.
    #[test]
    fn mixing_from_absent_is_a_pure_alpha_fade() {
        let grey = Color::from_rgb(0.4, 0.4, 0.4);
        let half = mix_color_opt(None, Some(grey), 0.5).unwrap();
        assert_eq!((half.r, half.g, half.b), (0.4, 0.4, 0.4));
        assert!((half.a - 0.5).abs() < 1e-6);
        assert_eq!(mix_color_opt(None, None, 0.5), None);
    }

    #[test]
    fn a_zero_width_border_mixes_as_transparent() {
        let a = Border { color: Color::from_rgb(1.0, 0.0, 0.0), width: 0.0, radius: 4.0.into() };
        let b = Border { color: Color::WHITE, width: 2.0, radius: 8.0.into() };
        let mid = mix_border(a, b, 0.5);
        assert_eq!((mid.color.r, mid.color.g, mid.color.b), (1.0, 1.0, 1.0));
        assert!((mid.width - 1.0).abs() < 1e-6);
        assert!((mid.radius.top_left - 6.0).abs() < 1e-6);
    }
}
