//! The sliding highlight behind a row of equal-width segments (the title
//! bar's tabs).
//!
//! The two edges are separate springs. When the selection moves, the leading
//! edge sets off on a quick spring and the trailing edge follows on a heavier
//! one, so the highlight stretches towards its destination mid-flight and
//! contracts as it arrives — the "liquid" motion that makes a segmented
//! control feel physical rather than merely animated.
//!
//! Draws only; put it as the *base* layer of a `stack` with the segment
//! buttons above it. The stack takes its size from this, so give the buttons
//! the same `segment_width`.

use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::event::{self, Event};
use iced::{mouse, window};
use iced::{Border, Color, Element, Length, Rectangle, Size, Theme};

use crate::ui::motion::{self, Animated, Spring};

type Renderer = iced::Renderer;

#[derive(Debug, Clone, Copy)]
pub struct Appearance {
    /// Fill behind the active segment.
    pub fill: Color,
    /// The accent bar along its bottom edge.
    pub accent: Color,
    pub accent_height: f32,
    /// Horizontal inset of the accent bar from the highlight's edges.
    pub accent_inset: f32,
}

pub struct Indicator {
    count: usize,
    active: usize,
    segment_width: f32,
    height: Length,
    appearance: Appearance,
}

pub fn indicator(count: usize, active: usize, segment_width: f32, appearance: Appearance) -> Indicator {
    Indicator {
        count: count.max(1),
        active: active.min(count.saturating_sub(1)),
        segment_width,
        height: Length::Fill,
        appearance,
    }
}

/// Edge positions in segment units: the highlight spans `[left, right]`.
#[derive(Debug, Clone, Copy)]
struct Edges {
    left: Animated,
    right: Animated,
    left_spring: Spring,
    right_spring: Spring,
    active: usize,
    clock: Option<Instant>,
}

impl Edges {
    fn at(active: usize) -> Self {
        Self {
            left: Animated::new(active as f32),
            right: Animated::new(active as f32 + 1.0),
            left_spring: Spring::SNAPPY,
            right_spring: Spring::SNAPPY,
            active,
            clock: None,
        }
    }

    fn select(&mut self, active: usize) {
        if active == self.active {
            return;
        }
        // The edge on the side of travel leads.
        let rightwards = active > self.active;
        (self.left_spring, self.right_spring) = if rightwards {
            (Spring::TRAIL, Spring::SNAPPY)
        } else {
            (Spring::SNAPPY, Spring::TRAIL)
        };
        self.left.set(active as f32);
        self.right.set(active as f32 + 1.0);
        self.active = active;
    }

    fn is_settled(&self) -> bool {
        self.left.is_settled() && self.right.is_settled()
    }

    fn step(&mut self, now: Instant) -> bool {
        let dt = motion::frame_dt(self.clock, now);
        self.clock = Some(now);
        let l = self.left.step(self.left_spring, dt);
        let r = self.right.step(self.right_spring, dt);
        // Overshoot on both springs could briefly cross the edges over; keep
        // the highlight at least a sliver wide.
        if self.right.value < self.left.value + 0.05 {
            self.right.value = self.left.value + 0.05;
        }
        if !(l || r) {
            self.clock = None;
        }
        l || r
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Indicator {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Edges>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Edges::at(self.active))
    }

    fn diff(&self, tree: &mut Tree) {
        tree.state.downcast_mut::<Edges>().select(self.active);
    }

    fn size(&self) -> Size<Length> {
        Size { width: Length::Fixed(self.segment_width * self.count as f32), height: self.height }
    }

    fn layout(&self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, self.segment_width * self.count as f32, self.height)
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
        let edges = tree.state.downcast_mut::<Edges>();
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            if edges.step(now) {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        } else if !edges.is_settled() {
            shell.request_redraw(window::RedrawRequest::NextFrame);
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
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let edges = tree.state.downcast_ref::<Edges>();
        let bounds = layout.bounds();
        let x0 = bounds.x + edges.left.value * self.segment_width;
        let x1 = bounds.x + edges.right.value * self.segment_width;
        let a = self.appearance;

        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle { x: x0, y: bounds.y, width: x1 - x0, height: bounds.height },
                ..renderer::Quad::default()
            },
            a.fill,
        );
        let inset = a.accent_inset.min((x1 - x0) / 3.0);
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x: x0 + inset,
                    y: bounds.y + bounds.height - a.accent_height,
                    width: (x1 - x0 - 2.0 * inset).max(0.0),
                    height: a.accent_height,
                },
                border: Border { radius: (a.accent_height / 2.0).into(), ..Border::default() },
                ..renderer::Quad::default()
            },
            a.accent,
        );
    }
}

impl<'a, Message: 'a> From<Indicator> for Element<'a, Message> {
    fn from(indicator: Indicator) -> Self {
        Self::new(indicator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Mid-flight the highlight is wider than one segment (it stretches), and
    /// it lands at exactly one segment wide on the new tab.
    #[test]
    fn the_highlight_stretches_in_flight_and_lands_on_the_new_segment() {
        let mut e = Edges::at(0);
        e.select(3);
        let mut now = Instant::now();
        let mut widest: f32 = 1.0;
        for _ in 0..120 {
            e.step(now);
            widest = widest.max(e.right.value - e.left.value);
            now += Duration::from_micros(16_667);
        }
        assert!(widest > 1.3, "never stretched (widest {widest})");
        assert!(e.is_settled());
        assert_eq!((e.left.value, e.right.value), (3.0, 4.0));
    }

    #[test]
    fn reselecting_the_same_segment_does_nothing() {
        let mut e = Edges::at(2);
        e.select(2);
        assert!(e.is_settled());
        assert!(!e.step(Instant::now()));
    }
}
