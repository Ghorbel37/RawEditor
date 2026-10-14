//! Wrappers that animate how their content *arrives*.
//!
//! iced 0.13 has transforms but no layer opacity, so a true fade of arbitrary
//! content isn't available. [`FadeThrough`] gets the same result where the
//! content sits on a known, uniform background: it draws a *veil* of that
//! colour over the content and fades the veil out. Fading from the
//! background colour to the content is exactly what a fade-in looks like.
//!
//! [`Entrance`] is for small things over arbitrary backgrounds (a badge on a
//! photo) where no veil colour exists: it pops them in on a lively spring
//! instead.

use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, Clipboard, Shell};
use iced::event::{self, Event};
use iced::{mouse, window};
use iced::{Color, Element, Length, Rectangle, Size, Theme, Transformation, Vector};

use crate::ui::motion::{self, ease, Animated, Spring, Timeline};

type Renderer = iced::Renderer;

// ── FadeThrough ─────────────────────────────────────────────────────────────

/// Fades `content` in from `background` whenever `key` changes — and once
/// when first shown. A pure fade: the content never moves, so the workspace
/// stays put under the user's eye while it resolves.
///
/// Used for the tab switch: the old tab is gone at once, the new one fades
/// up through the app's background. The outgoing content can't be kept (iced
/// can't render a widget tree that is no longer in the view), and a
/// fade-*through* is the transition that doesn't need it.
pub struct FadeThrough<'a, Message> {
    content: Element<'a, Message>,
    key: u64,
    background: Color,
}

pub fn fade_through<'a, Message>(
    key: u64,
    background: Color,
    content: impl Into<Element<'a, Message>>,
) -> FadeThrough<'a, Message> {
    FadeThrough { content: content.into(), key, background }
}

struct FadeState {
    key: u64,
    timeline: Timeline,
    now: Option<Instant>,
}

/// Veil alpha at linear progress `t`: fast to clear, gentle to finish.
fn fade_through_veil(t: f32) -> f32 {
    1.0 - ease::STANDARD.ease(t)
}

impl<'a, Message> Widget<Message, Theme, Renderer> for FadeThrough<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<FadeState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(FadeState { key: self.key, timeline: Timeline::armed(), now: None })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<FadeState>();
        if state.key != self.key {
            state.key = self.key;
            state.timeline = Timeline::armed();
        }
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let child = self.content.as_widget().layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(child.size(), vec![child])
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
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<FadeState>();
            state.now = Some(now);
            if state.timeline.tick(now, motion::duration::PAGE) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        }
        self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event,
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        )
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
        let state = tree.state.downcast_ref::<FadeState>();
        let t = state.timeline.progress(state.now, motion::duration::PAGE);
        let child_layout = layout.children().next().unwrap();
        let draw_child = |renderer: &mut Renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
        };
        if t >= 1.0 {
            draw_child(renderer);
            return;
        }
        draw_child(renderer);
        let veil = fade_through_veil(t);
        // A layer of its own so it lands on top of everything the content
        // drew — within one layer iced draws text after quads.
        let bounds = layout.bounds();
        renderer.with_layer(bounds, |renderer| {
            renderer.fill_quad(
                renderer::Quad { bounds, ..renderer::Quad::default() },
                motion::fade(self.background, veil),
            );
        });
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.children().next().unwrap(),
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<FadeThrough<'a, Message>> for Element<'a, Message> {
    fn from(w: FadeThrough<'a, Message>) -> Self {
        Self::new(w)
    }
}

// ── Entrance ────────────────────────────────────────────────────────────────

/// Pops `content` in on a lively spring the first time it is shown: from
/// `starting_scale` and `starting_offset` to rest, overshooting a touch.
pub struct Entrance<'a, Message> {
    content: Element<'a, Message>,
    starting_scale: f32,
    starting_offset: Vector,
}

pub fn entrance<'a, Message>(content: impl Into<Element<'a, Message>>) -> Entrance<'a, Message> {
    Entrance { content: content.into(), starting_scale: 0.85, starting_offset: Vector::new(0.0, 0.0) }
}

impl<'a, Message> Entrance<'a, Message> {
    pub fn starting_scale(mut self, scale: f32) -> Self {
        self.starting_scale = scale;
        self
    }

    pub fn starting_offset(mut self, x: f32, y: f32) -> Self {
        self.starting_offset = Vector::new(x, y);
        self
    }
}

struct EntranceState {
    /// 0 = at the starting pose, 1 = at rest.
    t: Animated,
    clock: Option<Instant>,
}

impl<'a, Message> Widget<Message, Theme, Renderer> for Entrance<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<EntranceState>()
    }

    fn state(&self) -> tree::State {
        let mut t = Animated::new(if motion::reduced() { 1.0 } else { 0.0 });
        t.set(1.0);
        tree::State::new(EntranceState { t, clock: None })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let child = self.content.as_widget().layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(child.size(), vec![child])
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
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<EntranceState>();
            let dt = motion::frame_dt(state.clock, now);
            state.clock = Some(now);
            if state.t.step(Spring::POP, dt) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        }
        self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event,
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        )
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
        let t = tree.state.downcast_ref::<EntranceState>().t.value;
        let child_layout = layout.children().next().unwrap();
        let draw_child = |renderer: &mut Renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
        };
        if (t - 1.0).abs() < 1e-4 {
            draw_child(renderer);
            return;
        }
        let scale = motion::lerp(self.starting_scale, 1.0, t).max(0.0);
        let c = layout.bounds().center();
        let off = self.starting_offset * (1.0 - t);
        let transform = Transformation::translate(c.x + off.x, c.y + off.y)
            * Transformation::scale(scale)
            * Transformation::translate(-c.x, -c.y);
        renderer.with_transformation(transform, draw_child);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.children().next().unwrap(),
            cursor,
            viewport,
            renderer,
        )
    }
}

impl<'a, Message: 'a> From<Entrance<'a, Message>> for Element<'a, Message> {
    fn from(w: Entrance<'a, Message>) -> Self {
        Self::new(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_through_starts_hidden_and_ends_clear() {
        assert_eq!(fade_through_veil(0.0), 1.0);
        assert!(fade_through_veil(1.0).abs() < 1e-6);
        // Front-loaded: most of the way there by the halfway point.
        assert!(fade_through_veil(0.5) < 0.2);
    }
}
