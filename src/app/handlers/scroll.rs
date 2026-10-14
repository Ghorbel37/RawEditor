// Wheel scroll boost (library grid) + filmstrip momentum.
//
// iced 0.13's scrollable hardcodes wheel scrolling at 60px/line with no
// configuration API.
//
// Library grid: we let iced apply its native scroll and nudge it further
// with `scroll_by`, while the grid's own mouse_area reports the cursor over it.
//
// Filmstrip: iced's Stack widget dispatches wheel events top-layer-first and
// stops at the first layer that captures them (see iced_widget::stack::
// on_event — `.find(Captured)` short-circuits a lazy iterator), so
// src/ui/filmstrip.rs stacks a transparent `mouse_area::on_scroll` ABOVE its
// scrollable. That interceptor captures every wheel event before the
// scrollable underneath ever sees it, fully suppressing iced's native jump —
// all filmstrip scrolling below is 100% our own physics, so there's no
// native contribution to blend with and no seam.
//
// The develop preview's own `mouse_area::on_scroll` already consumes wheel
// events for zoom, and everything else (sidebars, library folder list)
// deliberately keeps native speed.

use iced::mouse::ScrollDelta;
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::Task;

use crate::app::message::{AppTab, Message};
use crate::app::state::RawEditor;
use crate::app::state::{GlideTarget, Modal, ScrollGlide};
use crate::ui::motion::{self, Animated, Spring};

/// Extra multiplier on top of iced's native 60px/line.
const GRID_MULT: f32 = 4.0;

const STRIP_LINE_PX: f32 = 60.0;

// ── Filmstrip momentum physics (all deltaT-driven) ─────────────────────────
//
// Every wheel tick measures how fast the user is actually spinning the wheel
// (pixels-equivalent / real elapsed time since the last tick), and that
// measured speed drives two things: how much velocity gets added, and how
// heavy the resulting glide feels.

/// Clamp bounds for the deltaT used to estimate input speed: a floor so two
/// events arriving almost simultaneously (fast trackpad bursts) don't blow
/// up the rate, and a ceiling that also marks "this is a new gesture" (see
/// `is_new_gesture` below) rather than genuine continuous scrolling.
const MIN_WHEEL_DT: f32 = 0.008;
const GESTURE_GAP: f32 = 0.3;
/// EMA smoothing for the input-speed estimate, so a single outlier tick
/// doesn't spike the computed weight.
const INPUT_SMOOTH: f32 = 0.5;

/// Input speed (px/s) that maps to "fully fast" for the weight interpolation.
const SPEED_REF: f32 = 3500.0;
/// Velocity (px/s) added per (px/s) of measured input speed, per tick.
const IMPULSE_GAIN: f32 = 1.3;
const MAX_SPEED: f32 = 5000.0;
const STOP_SPEED: f32 = 15.0;

/// Decay range (1/s): slow input -> DECAY_SLOW (heavy, glides a long time
/// even though the input itself was gentle); fast input -> DECAY_FAST
/// (light — stops/redirects quickly so a fast flick stays controllable
/// instead of sailing past where you meant to land).
const DECAY_SLOW: f32 = 1.2;
const DECAY_FAST: f32 = 6.5;

/// Library grid boost, applied while the cursor is over the grid.
pub fn handle_wheel(editor: &mut RawEditor, delta: ScrollDelta) -> Task<Message> {
    if editor.active_modal != Modal::None || editor.current_tab != AppTab::Library {
        return Task::none();
    }
    let lines = match delta {
        ScrollDelta::Lines { y, .. } => y,
        ScrollDelta::Pixels { y, .. } => y / 60.0,
    };
    if lines == 0.0 {
        return Task::none();
    }

    if editor.cursor_over_grid {
        // The user has taken the wheel: a reveal glide stops where it is.
        cancel_glide(editor, GlideTarget::Grid);
        return scrollable::scroll_by(
            crate::app::views::library::grid_scroll_id(),
            AbsoluteOffset { x: 0.0, y: -(GRID_MULT - 1.0) * 60.0 * lines },
        );
    }
    Task::none()
}

/// Filmstrip wheel input, captured directly by its own overlay (native
/// scrolling never fires — see module docs) so this drives 100% of the motion.
pub fn handle_filmstrip_wheel(editor: &mut RawEditor, delta: ScrollDelta) -> Task<Message> {
    if editor.active_modal != Modal::None {
        return Task::none();
    }
    let lines = match delta {
        ScrollDelta::Lines { y, .. } => y,
        ScrollDelta::Pixels { y, .. } => y / 60.0,
    };
    if lines == 0.0 {
        return Task::none();
    }

    cancel_glide(editor, GlideTarget::Filmstrip);

    let now = std::time::Instant::now();
    let raw_dt = editor
        .last_wheel_time
        .map(|t| now.duration_since(t).as_secs_f32());
    editor.last_wheel_time = Some(now);

    // How fast is the wheel actually spinning right now?
    let dt_for_rate = raw_dt.unwrap_or(GESTURE_GAP).clamp(MIN_WHEEL_DT, GESTURE_GAP);
    let raw_rate = (lines.abs() * STRIP_LINE_PX) / dt_for_rate;
    // A real pause (or the very first tick) means the previous estimate is
    // stale — start fresh instead of blending into it.
    let is_new_gesture = raw_dt.map(|d| d >= GESTURE_GAP).unwrap_or(true);
    editor.filmstrip_input_speed = if is_new_gesture {
        raw_rate
    } else {
        editor.filmstrip_input_speed * (1.0 - INPUT_SMOOTH) + raw_rate * INPUT_SMOOTH
    }
    .min(SPEED_REF * 2.0);

    // Opposite direction: cancel the old glide instantly instead of
    // fighting it with a subtracted impulse.
    let dir = -lines.signum();
    if editor.filmstrip_velocity != 0.0 && editor.filmstrip_velocity.signum() != dir {
        editor.filmstrip_velocity = 0.0;
    }
    let impulse = dir * editor.filmstrip_input_speed * IMPULSE_GAIN;
    editor.filmstrip_velocity = (editor.filmstrip_velocity + impulse).clamp(-MAX_SPEED, MAX_SPEED);

    // Weight for this glide: fast input -> low weight (high decay, stops
    // quickly); slow input -> high weight (low decay, glides for a while).
    let t = (editor.filmstrip_input_speed / SPEED_REF).clamp(0.0, 1.0);
    editor.filmstrip_decay = DECAY_SLOW + (DECAY_FAST - DECAY_SLOW) * t;

    // No synchronous scroll_by here: velocity is already at its peak right
    // after the impulse (it only decays from here, never ramps up), so
    // letting the per-frame kinetic tick loop apply it from t=0 is already
    // smooth by construction — a synchronous "catch-up" chunk would just
    // front-load a disproportionately large jump relative to the small
    // per-frame steps that follow, recreating the exact discontinuity this
    // whole interceptor was built to eliminate.
    editor.last_kinetic_tick = Some(now);
    Task::none()
}

pub fn handle_kinetic_tick(editor: &mut RawEditor, now: std::time::Instant) -> Task<Message> {
    let glide = step_glide(editor, now);
    if editor.filmstrip_velocity == 0.0 {
        return glide;
    }
    // Tab switched mid-glide (the filmstrip may not even be mounted) — stop.
    if editor.current_tab == AppTab::Library {
        editor.filmstrip_velocity = 0.0;
        editor.last_kinetic_tick = None;
        return Task::none();
    }

    let dt = editor
        .last_kinetic_tick
        .map(|t| now.duration_since(t).as_secs_f32())
        .unwrap_or(1.0 / 60.0)
        .clamp(0.0, 0.05);
    editor.last_kinetic_tick = Some(now);

    let step = editor.filmstrip_velocity * dt;
    editor.filmstrip_velocity *= (-editor.filmstrip_decay * dt).exp();
    if editor.filmstrip_velocity.abs() < STOP_SPEED {
        editor.filmstrip_velocity = 0.0;
        editor.last_kinetic_tick = None;
    }

    scrollable::scroll_by(
        crate::ui::filmstrip::scroll_id(),
        AbsoluteOffset { x: step, y: 0.0 },
    )
}

// ── Reveal glide ───────────────────────────────────────────────────────────
//
// Keyboard navigation used to *jump* the grid / filmstrip to bring the new
// selection into view, which loses the user's sense of where they are. Now it
// glides there on a critically damped spring, driven by the same per-frame
// tick as the filmstrip momentum (`subscription` runs it while
// `scroll_glide` is set).

fn scroll_id(target: GlideTarget) -> scrollable::Id {
    match target {
        GlideTarget::Grid => crate::app::views::library::grid_scroll_id(),
        GlideTarget::Filmstrip => crate::ui::filmstrip::scroll_id(),
    }
}

fn current_offset(editor: &RawEditor, target: GlideTarget) -> f32 {
    match target {
        GlideTarget::Grid => editor.grid_scroll_y,
        GlideTarget::Filmstrip => editor.filmstrip_scroll_x,
    }
}

/// Record `offset` as the scrollable's position — ahead of its own
/// `on_scroll` report, so the very next view builds the rows/slots it is
/// scrolling to.
fn set_offset(editor: &mut RawEditor, target: GlideTarget, offset: f32) {
    match target {
        GlideTarget::Grid => editor.grid_scroll_y = offset,
        GlideTarget::Filmstrip => editor.filmstrip_scroll_x = offset,
    }
}

fn scroll_to(target: GlideTarget, offset: f32) -> Task<Message> {
    let offset = match target {
        GlideTarget::Grid => AbsoluteOffset { x: 0.0, y: offset },
        GlideTarget::Filmstrip => AbsoluteOffset { x: offset, y: 0.0 },
    };
    scrollable::scroll_to(scroll_id(target), offset)
}

/// Where an in-flight glide on `target` will come to rest.
pub fn glide_destination(editor: &RawEditor, target: GlideTarget) -> Option<f32> {
    editor.scroll_glide.filter(|g| g.target == target).map(|g| g.to)
}

/// A glide is done once within half a pixel of its destination and barely
/// moving; anything finer is invisible.
const GLIDE_REST_PX: f32 = 0.5;

/// Farthest a reveal glides, in viewports. Beyond it a glide is a blur of
/// thumbnails the user never wanted to see — each one loaded as it passes —
/// so it jumps instead.
const GLIDE_MAX_VIEWPORTS: f32 = 1.5;

/// The visible extent of `target` along its scroll axis.
fn viewport_extent(editor: &RawEditor, target: GlideTarget) -> f32 {
    let (reported, window) = match target {
        GlideTarget::Grid => (editor.grid_viewport.1, editor.window_size.height),
        GlideTarget::Filmstrip => (editor.filmstrip_view_w, editor.window_size.width),
    };
    if reported > 0.0 {
        reported
    } else {
        window
    }
}

/// Scroll `target` to `offset`: a glide if `animate` and it is a short
/// move, otherwise a jump (also under reduced motion). A glide already in
/// flight is re-aimed without losing its speed.
pub fn glide_to(
    editor: &mut RawEditor,
    target: GlideTarget,
    offset: f32,
    animate: bool,
) -> Task<Message> {
    let offset = offset.max(0.0);
    let from = editor
        .scroll_glide
        .filter(|g| g.target == target)
        .map_or_else(|| current_offset(editor, target), |g| g.offset());
    let short = (offset - from).abs() <= GLIDE_MAX_VIEWPORTS * viewport_extent(editor, target);
    if !animate || !short || motion::reduced() {
        editor.scroll_glide = None;
        set_offset(editor, target, offset);
        return scroll_to(target, offset);
    }
    match &mut editor.scroll_glide {
        // Re-aim: same position and velocity, measured from the new end.
        Some(glide) if glide.target == target => {
            glide.displacement.value = glide.offset() - offset;
            glide.to = offset;
        }
        _ => {
            let mut displacement = Animated::new(current_offset(editor, target) - offset);
            displacement.set(0.0);
            editor.scroll_glide =
                Some(ScrollGlide { target, to: offset, displacement, last_tick: None });
        }
    }
    Task::none()
}

fn cancel_glide(editor: &mut RawEditor, target: GlideTarget) {
    if editor.scroll_glide.is_some_and(|g| g.target == target) {
        editor.scroll_glide = None;
    }
}

fn step_glide(editor: &mut RawEditor, now: std::time::Instant) -> Task<Message> {
    let Some(mut glide) = editor.scroll_glide else {
        return Task::none();
    };
    // The scrollable may not even be mounted any more.
    let mounted = match glide.target {
        GlideTarget::Grid => editor.current_tab == AppTab::Library,
        GlideTarget::Filmstrip => matches!(editor.current_tab, AppTab::Cull | AppTab::Develop),
    };
    if !mounted || editor.active_modal != Modal::None {
        editor.scroll_glide = None;
        return Task::none();
    }
    let dt = motion::frame_dt(glide.last_tick, now);
    glide.last_tick = Some(now);
    let moving = glide.displacement.step_until(Spring::SMOOTH, dt, GLIDE_REST_PX);
    let offset = glide.offset().max(0.0);
    editor.scroll_glide = moving.then_some(glide);
    set_offset(editor, glide.target, offset);
    scroll_to(glide.target, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> RawEditor {
        RawEditor::new().0
    }

    /// Re-aiming a glide keeps one continuous motion rather than restarting.
    #[test]
    fn re_aiming_a_glide_keeps_its_speed() {
        let mut e = editor();
        e.current_tab = AppTab::Develop;
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 1000.0, true);
        let start = std::time::Instant::now();
        for i in 0..6 {
            let _ = step_glide(&mut e, start + std::time::Duration::from_millis(16 * i));
        }
        let before = e.scroll_glide.unwrap();
        // Displacement shrinks towards 0 from below: moving forwards.
        assert!(before.displacement.velocity > 0.0);
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 1200.0, true);
        let after = e.scroll_glide.unwrap();
        assert_eq!(after.displacement.velocity, before.displacement.velocity);
        assert_eq!(after.to, 1200.0);
        assert!((after.offset() - before.offset()).abs() < 1e-3, "position jumped");
    }

    /// The regression behind a crash: at six-figure offsets (a long
    /// filmstrip) a spring on the raw offset stalled a fraction of a pixel
    /// short and glided forever, issuing `scroll_to` every frame — which
    /// panicked iced_aw the moment a dropdown was opened.
    #[test]
    fn a_glide_ends_even_at_huge_offsets() {
        let mut e = editor();
        e.current_tab = AppTab::Develop;
        e.filmstrip_scroll_x = 171_000.0;
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 172_345.67, true);
        let start = std::time::Instant::now();
        let mut frames = 0;
        while e.scroll_glide.is_some() {
            frames += 1;
            assert!(frames < 120, "glide never ended: {:?}", e.scroll_glide);
            let _ = step_glide(&mut e, start + std::time::Duration::from_micros(16_667 * frames));
        }
        assert_eq!(e.filmstrip_scroll_x, 172_345.67);
    }

    #[test]
    fn a_glide_lands_on_its_target_and_ends() {
        let mut e = editor();
        e.current_tab = AppTab::Library;
        let _ = glide_to(&mut e, GlideTarget::Grid, 640.0, true);
        let start = std::time::Instant::now();
        let mut frames = 0;
        while e.scroll_glide.is_some() {
            frames += 1;
            assert!(frames < 240, "glide never ended");
            let _ = step_glide(&mut e, start + std::time::Duration::from_micros(16_667 * frames));
        }
        assert_eq!(e.grid_scroll_y, 640.0);
    }

    /// A long reveal — or any reveal on a tab switch — jumps rather than
    /// sweeping past every thumbnail in between.
    #[test]
    fn long_or_instant_reveals_jump() {
        let mut e = editor();
        e.current_tab = AppTab::Develop;
        e.filmstrip_view_w = 1600.0;
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 50_000.0, true);
        assert!(e.scroll_glide.is_none());
        assert_eq!(e.filmstrip_scroll_x, 50_000.0);

        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 50_400.0, false);
        assert!(e.scroll_glide.is_none());
        assert_eq!(e.filmstrip_scroll_x, 50_400.0);

        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 51_000.0, true);
        assert!(e.scroll_glide.is_some(), "a short in-place reveal still glides");
    }

    #[test]
    fn wheel_input_cancels_a_glide() {
        let mut e = editor();
        e.current_tab = AppTab::Develop;
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 500.0, true);
        let _ = handle_filmstrip_wheel(&mut e, ScrollDelta::Lines { x: 0.0, y: -1.0 });
        assert!(e.scroll_glide.is_none());
    }

    #[test]
    fn leaving_the_tab_drops_the_glide() {
        let mut e = editor();
        e.current_tab = AppTab::Develop;
        let _ = glide_to(&mut e, GlideTarget::Filmstrip, 500.0, true);
        e.current_tab = AppTab::Library;
        let _ = step_glide(&mut e, std::time::Instant::now());
        assert!(e.scroll_glide.is_none());
    }
}

