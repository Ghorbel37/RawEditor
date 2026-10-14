//! An image that fades in when it arrives and crossfades when it changes.
//!
//! For thumbnails: a camera preview finishing its cache tier fades up over
//! the placeholder instead of popping in, and an edited proxy replacing the
//! camera JPEG (after the user leaves Develop) dissolves from the old look to
//! the new one.
//!
//! Only *changes observed while mounted* animate. A widget built already
//! holding its image shows it at once, and a change of `key` (a virtualised
//! slot now showing a different photo) snaps — otherwise scrolling the grid
//! would set every thumbnail fading.

use std::time::Instant;

use iced::advanced::image as core_image;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::event::{self, Event};
use iced::widget::image::Handle;
use iced::{mouse, window};
use iced::{ContentFit, Element, Length, Rectangle, Rotation, Size, Theme};

use crate::ui::motion::{self, ease, Timeline};

type Renderer = iced::Renderer;

pub struct FadeImage {
    handle: Option<Handle>,
    key: u64,
    width: Length,
    height: Length,
    fit: ContentFit,
}

/// `key` is the identity of what is shown (the image id).
pub fn fade_image(key: u64, handle: Option<Handle>, width: f32, height: f32) -> FadeImage {
    FadeImage {
        handle,
        key,
        width: Length::Fixed(width),
        height: Length::Fixed(height),
        fit: ContentFit::Contain,
    }
}

impl FadeImage {
    pub fn content_fit(mut self, fit: ContentFit) -> Self {
        self.fit = fit;
        self
    }
}

struct State {
    key: u64,
    shown: Option<Handle>,
    /// What `shown` is replacing, drawn beneath it until the fade completes.
    previous: Option<Handle>,
    fade: Timeline,
    now: Option<Instant>,
}

fn same(a: &Option<Handle>, b: &Option<Handle>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id() == b.id(),
        (None, None) => true,
        _ => false,
    }
}

impl<Message> Widget<Message, Theme, Renderer> for FadeImage {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            key: self.key,
            shown: self.handle.clone(),
            previous: None,
            fade: Timeline::Idle,
            now: None,
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.key != self.key {
            *state = State {
                key: self.key,
                shown: self.handle.clone(),
                previous: None,
                fade: Timeline::Idle,
                now: state.now,
            };
        } else if !same(&state.shown, &self.handle) {
            // Develop refreshes the open image's proxy on every render, so
            // during a slider drag changes arrive each frame. Only an isolated
            // change crossfades; one landing mid-fade just shows.
            if state.fade.is_idle() {
                state.previous = state.shown.take();
                state.fade = Timeline::armed();
            } else {
                state.previous = None;
                state.fade = Timeline::Idle;
            }
            state.shown = self.handle.clone();
        }
    }

    fn size(&self) -> Size<Length> {
        Size { width: self.width, height: self.height }
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<State>();
            state.now = Some(now);
            if state.fade.tick(now, motion::duration::FADE) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            } else {
                state.previous = None;
            }
        }
        event::Status::Ignored
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if !layout.bounds().intersects(viewport) {
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        let opacity = ease::STANDARD.ease(state.fade.progress(state.now, motion::duration::FADE));
        let mut draw = |handle: &Handle, opacity: f32| {
            iced::widget::image::draw(
                renderer,
                layout,
                handle,
                self.fit,
                core_image::FilterMethod::Linear,
                Rotation::default(),
                opacity,
            );
        };
        if opacity < 1.0 {
            if let Some(previous) = &state.previous {
                draw(previous, 1.0);
            }
        }
        if let Some(shown) = &state.shown {
            draw(shown, opacity);
        }
    }
}

impl<'a, Message> From<FadeImage> for Element<'a, Message> {
    fn from(w: FadeImage) -> Self {
        Self::new(w)
    }
}
