//! The detail loupe's drag surface (`handlers::loupe`).
//!
//! Sits over the loupe image and turns press-and-drag into
//! `Message::LoupePan`. It emits nothing while the pointer merely hovers: a
//! message per mouse move rebuilds the whole view (CLAUDE.md §E), so only an
//! actual drag — which, like a slider's, is meant to update — sends any.

use iced::mouse::{self, Cursor};
use iced::widget::canvas;
use iced::{Point, Rectangle};

use crate::app::message::Message;

pub struct LoupeDrag;

impl canvas::Program<Message> for LoupeDrag {
    /// The pointer's last position while a drag is in progress.
    type State = Option<Point>;

    fn update(
        &self,
        state: &mut Self::State,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        use canvas::event::Status;
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if cursor.is_over(bounds) => {
                *state = cursor.position();
                (Status::Captured, None)
            }
            // Followed past the loupe's edge too: the canvas sees every move.
            canvas::Event::Mouse(mouse::Event::CursorMoved { position }) if state.is_some() => {
                let last = state.replace(position).unwrap_or(position);
                (Status::Captured, Some(Message::LoupePan(position.x - last.x, position.y - last.y)))
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.is_some() => {
                *state = None;
                (Status::Captured, None)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        _renderer: &iced::Renderer,
        _theme: &iced::Theme,
        _bounds: Rectangle,
        _cursor: Cursor,
    ) -> Vec<canvas::Geometry> {
        Vec::new()
    }

    fn mouse_interaction(&self, state: &Self::State, bounds: Rectangle, cursor: Cursor) -> mouse::Interaction {
        if state.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}
