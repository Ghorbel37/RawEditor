//! Modal choreography: a [`Scrim`] that dims the app, and a [`Sheet`] — the
//! dialog card — that arrives and leaves with weight.
//!
//! **Entrance** (one timeline, staggered):
//! 1. the scrim fades up while the card's *surface* fades in, rising from a
//!    few pixels below at 95% scale on an emphasized curve: fast to arrive,
//!    unhurried to settle;
//! 2. once the surface is opaque, the *content* fades in on top of it.
//!
//! The Sheet draws the surface itself (not via a styled container) precisely
//! so the surface can truly fade — iced 0.13 has no layer opacity. Content
//! can't be faded that way, so it is revealed from under a *veil* of the
//! surface colour, which is identical to a fade once the surface is opaque;
//! hence the stagger.
//!
//! **Exit** is quicker and simpler: content disappears at once, the surface
//! sinks slightly and dissolves, the scrim lifts. The app keeps the closing
//! modal mounted (with empty content) for [`duration::MODAL_EXIT`] — see
//! `RawEditor::modal_exit` — and the Sheet keeps its last size meanwhile, so
//! the ghost card never jumps.
//!
//! Both are inert while exiting: the user has already dismissed the dialog,
//! and the app beneath must respond to them at once.

use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, Clipboard, Shell};
use iced::event::{self, Event};
use iced::{mouse, window};
use iced::{
    Border, Color, Element, Length, Padding, Rectangle, Shadow, Size, Theme, Transformation,
    Vector,
};

use crate::ui::motion::{self, duration, ease, Timeline};

type Renderer = iced::Renderer;

/// Where a modal is in its life. `P` is whatever the widget animates.
#[derive(Debug, Clone, Copy)]
enum Phase<P> {
    Entering(Timeline),
    /// `from` is the pose at the moment the exit began, so a dialog closed
    /// mid-entrance leaves from where it actually was.
    Exiting { timeline: Timeline, from: P },
}

impl<P: Copy> Phase<P> {
    fn timeline_mut(&mut self) -> &mut Timeline {
        match self {
            Self::Entering(t) | Self::Exiting { timeline: t, .. } => t,
        }
    }

    fn is_exiting(&self) -> bool {
        matches!(self, Self::Exiting { .. })
    }

    fn duration(&self) -> f32 {
        if self.is_exiting() {
            duration::MODAL_EXIT
        } else {
            duration::MODAL_ENTER
        }
    }

    /// Follow the view's `exiting` flag. Re-opening during an exit re-enters.
    fn follow(&mut self, exiting: bool, current: P) {
        match (self.is_exiting(), exiting) {
            (false, true) => {
                *self = Self::Exiting { timeline: Timeline::armed(), from: current }
            }
            (true, false) => *self = Self::Entering(Timeline::armed()),
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        let d = self.duration();
        self.timeline_mut().tick(now, d)
    }
}

// ── Scrim ───────────────────────────────────────────────────────────────────

/// The dimmed, click-to-dismiss backdrop. Fills its bounds.
pub struct Scrim<Message> {
    on_press: Message,
    exiting: bool,
    max_alpha: f32,
}

pub fn scrim<Message>(on_press: Message, exiting: bool) -> Scrim<Message> {
    Scrim { on_press, exiting, max_alpha: 0.55 }
}

struct ScrimState {
    phase: Phase<f32>,
    now: Option<Instant>,
}

/// Scrim alpha as a fraction of its maximum.
fn scrim_level(phase: &Phase<f32>, now: Option<Instant>) -> f32 {
    match phase {
        Phase::Entering(t) => {
            let p = t.progress(now, duration::MODAL_ENTER);
            ease::STANDARD.ease(motion::segment(p, 0.0, 0.6))
        }
        Phase::Exiting { timeline, from } => {
            from * (1.0 - ease::EXIT.ease(timeline.progress(now, duration::MODAL_EXIT)))
        }
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Scrim<Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ScrimState>()
    }

    fn state(&self) -> tree::State {
        let phase = if self.exiting {
            Phase::Exiting { timeline: Timeline::Idle, from: 0.0 }
        } else {
            Phase::Entering(Timeline::armed())
        };
        tree::State::new(ScrimState { phase, now: None })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<ScrimState>();
        let level = scrim_level(&state.phase, state.now);
        state.phase.follow(self.exiting, level);
    }

    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        let state = tree.state.downcast_mut::<ScrimState>();
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            state.now = Some(now);
            if state.phase.tick(now) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
            return event::Status::Ignored;
        }
        if self.exiting || !cursor.is_over(layout.bounds()) {
            return event::Status::Ignored;
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                shell.publish(self.on_press.clone());
                event::Status::Captured
            }
            // Nothing beneath an open dialog should react to the pointer —
            // a wheel over the dim used to scroll the grid behind it.
            Event::Mouse(
                mouse::Event::ButtonPressed(_)
                | mouse::Event::ButtonReleased(_)
                | mouse::Event::WheelScrolled { .. },
            ) => event::Status::Captured,
            _ => event::Status::Ignored,
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let state = tree.state.downcast_ref::<ScrimState>();
        let level = scrim_level(&state.phase, state.now);
        if level <= 0.001 {
            return;
        }
        renderer.fill_quad(
            renderer::Quad { bounds: layout.bounds(), ..renderer::Quad::default() },
            Color::from_rgba(0.0, 0.0, 0.0, self.max_alpha * level),
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        // Claiming the pointer makes the `stack` hand an unavailable cursor
        // to the layers beneath, so nothing behind the dialog shows hover.
        if !self.exiting && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: Clone + 'a> From<Scrim<Message>> for Element<'a, Message> {
    fn from(w: Scrim<Message>) -> Self {
        Self::new(w)
    }
}

// ── Sheet ───────────────────────────────────────────────────────────────────

/// The card's surface, drawn by the Sheet itself.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    pub background: Color,
    pub border: Border,
    pub shadow: Shadow,
}

pub struct Sheet<'a, Message> {
    content: Element<'a, Message>,
    padding: Padding,
    surface: Surface,
    key: u64,
    exiting: bool,
}

/// `key` identifies which dialog this is: a different key re-runs the
/// entrance even if a sheet is already on screen.
pub fn sheet<'a, Message>(
    key: u64,
    exiting: bool,
    surface: Surface,
    content: impl Into<Element<'a, Message>>,
) -> Sheet<'a, Message> {
    Sheet { content: content.into(), padding: Padding::new(20.0), surface, key, exiting }
}

/// Everything the Sheet animates, as one value.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pose {
    surface: f32,
    content: f32,
    scale: f32,
    /// Vertical offset in px (positive = below its resting place).
    dy: f32,
}

const REST: Pose = Pose { surface: 1.0, content: 1.0, scale: 1.0, dy: 0.0 };
const ENTER_SCALE: f32 = 0.95;
const ENTER_RISE: f32 = 14.0;
const EXIT_SCALE: f32 = 0.97;
const EXIT_SINK: f32 = 6.0;
/// The content starts once the surface is fully opaque (see module docs).
const SURFACE_END: f32 = 0.3;

fn enter_pose(t: f32) -> Pose {
    let e = ease::EMPHASIZED.ease(t);
    Pose {
        surface: ease::STANDARD.ease(motion::segment(t, 0.0, SURFACE_END)),
        content: ease::STANDARD.ease(motion::segment(t, SURFACE_END, 0.9)),
        scale: motion::lerp(ENTER_SCALE, 1.0, e),
        dy: ENTER_RISE * (1.0 - e),
    }
}

fn exit_pose(from: Pose, t: f32) -> Pose {
    let e = ease::EXIT.ease(t);
    Pose {
        surface: from.surface * (1.0 - e),
        content: 0.0,
        scale: motion::lerp(from.scale, EXIT_SCALE, e),
        dy: motion::lerp(from.dy, EXIT_SINK, e),
    }
}

struct SheetState {
    key: u64,
    phase: Phase<Pose>,
    now: Option<Instant>,
    /// Size as last laid out while open, kept for the exit (whose content is
    /// empty).
    size: Size,
}

impl SheetState {
    fn pose(&self) -> Pose {
        match self.phase {
            Phase::Entering(t) => enter_pose(t.progress(self.now, duration::MODAL_ENTER)),
            Phase::Exiting { timeline, from } => {
                exit_pose(from, timeline.progress(self.now, duration::MODAL_EXIT))
            }
        }
    }
}

impl<'a, Message> Widget<Message, Theme, Renderer> for Sheet<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SheetState>()
    }

    fn state(&self) -> tree::State {
        let phase = if self.exiting {
            Phase::Exiting { timeline: Timeline::Idle, from: REST }
        } else {
            Phase::Entering(Timeline::armed())
        };
        tree::State::new(SheetState { key: self.key, phase, now: None, size: Size::ZERO })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<SheetState>();
        if state.key != self.key && !self.exiting {
            state.key = self.key;
            state.phase = Phase::Entering(Timeline::armed());
        } else {
            let pose = state.pose();
            state.phase.follow(self.exiting, pose);
        }
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size { width: Length::Shrink, height: Length::Shrink }
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let node = layout::padded(limits, Length::Shrink, Length::Shrink, self.padding, |limits| {
            self.content.as_widget().layout(&mut tree.children[0], renderer, limits)
        });
        let state = tree.state.downcast_mut::<SheetState>();
        if self.exiting && state.size != Size::ZERO {
            let children = node.children().to_vec();
            layout::Node::with_children(state.size, children)
        } else {
            state.size = node.size();
            node
        }
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget().operate(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            operation,
        );
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> event::Status {
        let state = tree.state.downcast_mut::<SheetState>();
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            state.now = Some(now);
            if state.phase.tick(now) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        }
        if self.exiting {
            return event::Status::Ignored;
        }
        let status = self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event.clone(),
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        // Swallow clicks on the card itself so they don't reach the scrim
        // and dismiss the dialog.
        if status == event::Status::Ignored
            && cursor.is_over(layout.bounds())
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::ButtonReleased(_))
            )
        {
            return event::Status::Captured;
        }
        status
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let state = tree.state.downcast_ref::<SheetState>();
        let pose = state.pose();
        if pose.surface <= 0.001 {
            return;
        }
        let bounds = layout.bounds();
        let s = self.surface;

        let draw_sheet = |renderer: &mut Renderer| {
            // The shadow grows as the card lifts into place.
            let lift = 0.4 + 0.6 * pose.surface;
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: Border { color: motion::fade(s.border.color, pose.surface), ..s.border },
                    shadow: Shadow {
                        color: motion::fade(s.shadow.color, pose.surface),
                        offset: s.shadow.offset * lift,
                        blur_radius: s.shadow.blur_radius * lift,
                    },
                },
                motion::fade(s.background, pose.surface),
            );
            if pose.content <= 0.001 {
                return;
            }
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout.children().next().unwrap(),
                cursor,
                viewport,
            );
            if pose.content < 1.0 {
                // Inside the border, so the border stays crisp while the
                // content comes up.
                let w = s.border.width;
                let inner = Rectangle {
                    x: bounds.x + w,
                    y: bounds.y + w,
                    width: (bounds.width - 2.0 * w).max(0.0),
                    height: (bounds.height - 2.0 * w).max(0.0),
                };
                let r = (s.border.radius.top_left - w).max(0.0);
                renderer.with_layer(bounds, |renderer| {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: inner,
                            border: Border { radius: r.into(), ..Border::default() },
                            ..renderer::Quad::default()
                        },
                        motion::fade(s.background, 1.0 - pose.content),
                    );
                });
            }
        };

        if pose.scale == 1.0 && pose.dy == 0.0 {
            draw_sheet(renderer);
        } else {
            let c = bounds.center();
            let transform = Transformation::translate(c.x, c.y + pose.dy)
                * Transformation::scale(pose.scale)
                * Transformation::translate(-c.x, -c.y);
            renderer.with_transformation(transform, draw_sheet);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.exiting {
            return mouse::Interaction::None;
        }
        let inner = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.children().next().unwrap(),
            cursor,
            viewport,
            renderer,
        );
        if inner == mouse::Interaction::None && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
        } else {
            inner
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        if self.exiting {
            return None;
        }
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Sheet<'a, Message>> for Element<'a, Message> {
    fn from(w: Sheet<'a, Message>) -> Self {
        Self::new(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrance_runs_from_hidden_to_rest() {
        let start = enter_pose(0.0);
        assert_eq!((start.surface, start.content), (0.0, 0.0));
        assert_eq!(start.scale, ENTER_SCALE);
        assert_eq!(start.dy, ENTER_RISE);
        let end = enter_pose(1.0);
        assert_eq!((end.surface, end.content), (1.0, 1.0));
        assert!((end.scale - 1.0).abs() < 1e-5 && end.dy.abs() < 1e-4);
    }

    /// The veil trick only works over an opaque surface, so content must not
    /// start fading in until the surface has finished.
    #[test]
    fn content_waits_for_an_opaque_surface() {
        for i in 0..=100 {
            let p = enter_pose(i as f32 / 100.0);
            if p.content > 0.0 {
                assert!(p.surface >= 0.999, "content {} over surface {}", p.content, p.surface);
            }
        }
    }

    #[test]
    fn exit_leaves_from_where_the_entrance_was() {
        let mid = enter_pose(0.15);
        let p = exit_pose(mid, 0.0);
        assert_eq!(p.surface, mid.surface);
        assert_eq!(p.content, 0.0, "content hides at once on exit");
        assert_eq!((p.scale, p.dy), (mid.scale, mid.dy));
        let end = exit_pose(REST, 1.0);
        assert_eq!(end.surface, 0.0);
        assert_eq!((end.scale, end.dy), (EXIT_SCALE, EXIT_SINK));
    }

    #[test]
    fn closing_then_reopening_re_enters() {
        let mut phase: Phase<Pose> = Phase::Entering(Timeline::Idle);
        phase.follow(true, REST);
        assert!(phase.is_exiting());
        phase.follow(false, REST);
        assert!(matches!(phase, Phase::Entering(Timeline::Armed)));
    }
}
