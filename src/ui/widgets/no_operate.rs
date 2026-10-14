//! Shields a widget — and the overlays it opens — from widget *operations*.
//!
//! iced_aw 0.10's menu bar panics ("No menu defined in this item") when any
//! operation reaches it while a dropdown is open: its overlay's `operate`
//! recurses into every entry and `expect`s a submenu on each, which plain
//! entries never have. Operations are what `scrollable::scroll_to` /
//! `scroll_by` tasks are, so a filmstrip momentum glide still coasting when a
//! menu was opened took the app down.
//!
//! The menu bar holds nothing an operation could want (no addressed
//! scrollables, no text inputs), so this wrapper simply doesn't forward
//! them. Otherwise it is transparent — it even shares its content's tree
//! identity, like `container` — so it changes nothing else about the widget.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, Clipboard, Shell};
use iced::event::{self, Event};
use iced::mouse;
use iced::{Element, Length, Point, Rectangle, Size, Theme, Vector};

type Renderer = iced::Renderer;

pub struct NoOperate<'a, Message> {
    content: Element<'a, Message>,
}

pub fn no_operate<'a, Message>(content: impl Into<Element<'a, Message>>) -> NoOperate<'a, Message> {
    NoOperate { content: content.into() }
}

impl<'a, Message> Widget<Message, Theme, Renderer> for NoOperate<'a, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget().layout(tree, renderer, limits)
    }

    fn operate(
        &self,
        _tree: &mut Tree,
        _layout: Layout<'_>,
        _renderer: &Renderer,
        _operation: &mut dyn Operation,
    ) {
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
        self.content
            .as_widget_mut()
            .on_event(tree, event, layout, cursor, renderer, clipboard, shell, viewport)
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
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, translation)
            .map(guard)
    }
}

impl<'a, Message: 'a> From<NoOperate<'a, Message>> for Element<'a, Message> {
    fn from(w: NoOperate<'a, Message>) -> Self {
        Self::new(w)
    }
}

/// Wrap an overlay (and, recursively, any overlay it opens) so operations
/// stop at it.
fn guard<'b, Message: 'b>(
    inner: overlay::Element<'b, Message, Theme, Renderer>,
) -> overlay::Element<'b, Message, Theme, Renderer> {
    overlay::Element::new(Box::new(GuardedOverlay { inner }))
}

struct GuardedOverlay<'b, Message> {
    inner: overlay::Element<'b, Message, Theme, Renderer>,
}

impl<'b, Message: 'b> overlay::Overlay<Message, Theme, Renderer> for GuardedOverlay<'b, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.inner.layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.inner.draw(renderer, theme, style, layout, cursor);
    }

    // `operate` keeps the trait's default: do nothing. That is the point.

    fn on_event(
        &mut self,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> event::Status {
        self.inner.on_event(event, layout, cursor, renderer, clipboard, shell)
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.inner.mouse_interaction(layout, cursor, viewport, renderer)
    }

    fn is_over(&self, layout: Layout<'_>, renderer: &Renderer, cursor_position: Point) -> bool {
        self.inner.is_over(layout, renderer, cursor_position)
    }

    fn overlay<'c>(
        &'c mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'c, Message, Theme, Renderer>> {
        self.inner.overlay(layout, renderer).map(guard)
    }
}
