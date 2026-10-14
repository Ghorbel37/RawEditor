//! An accordion body: animates its height between zero and its content's
//! natural height, clipping the content as it unrolls, so everything below
//! slides smoothly instead of jumping.
//!
//! The height runs on a critically damped spring (no overshoot: a section
//! that briefly grows past its content would show a gap). On a known, uniform
//! background, pass it as `veil` and the content also fades in as it opens —
//! see `transition` for why a veil is how fades are done here.
//!
//! Keep building the content while collapsed; it is what the collapse
//! animates away. It is laid out but neither drawn nor sent events once shut.

use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, Clipboard, Shell};
use iced::event::{self, Event};
use iced::{mouse, window};
use iced::{Color, Element, Length, Rectangle, Size, Theme, Vector};

use crate::ui::motion::{self, ease, Animated, Spring};

type Renderer = iced::Renderer;

pub struct Collapse<'a, Message> {
    content: Element<'a, Message>,
    expanded: bool,
    veil: Option<Color>,
}

pub fn collapse<'a, Message>(
    expanded: bool,
    content: impl Into<Element<'a, Message>>,
) -> Collapse<'a, Message> {
    Collapse { content: content.into(), expanded, veil: None }
}

impl<'a, Message> Collapse<'a, Message> {
    /// The background the section sits on, to fade the content in from.
    pub fn veil(mut self, background: Color) -> Self {
        self.veil = Some(background);
        self
    }
}

struct State {
    /// Open fraction: 0 shut, 1 open.
    open: Animated,
    clock: Option<Instant>,
}

impl State {
    fn is_open(&self) -> bool {
        self.open.target >= 0.5
    }

    /// Fully open and at rest: the only time the content is interactive, so
    /// nothing can be clicked while it is sliding under the pointer.
    fn is_settled_open(&self) -> bool {
        self.is_open() && self.open.is_settled()
    }

    fn fraction(&self) -> f32 {
        self.open.value.clamp(0.0, 1.0)
    }
}

impl<'a, Message> Widget<Message, Theme, Renderer> for Collapse<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State { open: Animated::new(f32::from(u8::from(self.expanded))), clock: None })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.is_open() != self.expanded {
            state.open.set(f32::from(u8::from(self.expanded)));
        }
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Shrink }
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let max = limits.max();
        let child_limits =
            layout::Limits::new(Size::ZERO, Size::new(max.width, f32::INFINITY)).width(Length::Fill);
        let child = self.content.as_widget().layout(&mut tree.children[0], renderer, &child_limits);
        let fraction = tree.state.downcast_ref::<State>().fraction();
        let size = limits.resolve(
            Length::Fill,
            Length::Shrink,
            Size::new(child.size().width, child.size().height * fraction),
        );
        layout::Node::with_children(size, vec![child])
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
        let state = tree.state.downcast_mut::<State>();
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let before = state.fraction();
            let dt = motion::frame_dt(state.clock, now);
            state.clock = Some(now);
            let moving = state.open.step(Spring::SMOOTH, dt);
            if !moving {
                state.clock = None;
            }
            if state.fraction() != before {
                shell.invalidate_layout();
            }
            if moving {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        } else if !state.open.is_settled() {
            shell.request_redraw(window::RedrawRequest::NextFrame);
        }

        let interactive = state.is_settled_open();
        let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_)));
        // Redraws still reach an opening section, so animated children inside
        // it (buttons) keep their clocks running.
        if interactive || (is_redraw && state.is_open()) {
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
        } else {
            event::Status::Ignored
        }
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
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        if bounds.height < 0.5 {
            return;
        }
        let child_layout = layout.children().next().unwrap();
        if state.is_settled_open() {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
            return;
        }
        let fraction = state.fraction();
        renderer.with_layer(bounds, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                mouse::Cursor::Unavailable,
                &bounds,
            );
        });
        if let Some(background) = self.veil {
            let veil = 1.0 - ease::STANDARD.ease(fraction);
            if veil > 0.001 {
                renderer.with_layer(bounds, |renderer| {
                    renderer.fill_quad(
                        renderer::Quad { bounds, ..renderer::Quad::default() },
                        motion::fade(background, veil),
                    );
                });
            }
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
        if !tree.state.downcast_ref::<State>().is_settled_open() {
            return mouse::Interaction::None;
        }
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
        if !tree.state.downcast_ref::<State>().is_settled_open() {
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

impl<'a, Message: 'a> From<Collapse<'a, Message>> for Element<'a, Message> {
    fn from(w: Collapse<'a, Message>) -> Self {
        Self::new(w)
    }
}
