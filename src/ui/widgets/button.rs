//! A drop-in replacement for `iced::widget::button` whose style *transitions*
//! instead of switching.
//!
//! Same builder API and the same `Fn(&Theme, Status) -> Style` style
//! functions, so every existing style in `ui::styles` keeps working unchanged;
//! swapping the import is the whole migration. What changes is the feel:
//!
//! - **Hover / press colours ease** rather than snap. Asymmetrically: they
//!   come on almost instantly (the UI must feel like it heard you) and fade
//!   off more slowly (the trail is what reads as polish).
//! - **Style swaps ease too.** When the view hands the button a different
//!   style — a tab becoming active, Neutral → Accent when a tool arms — the
//!   button crossfades from what it was showing to the new look. It detects
//!   this by evaluating the style function itself, so callers need no
//!   bookkeeping.
//! - **Pressing depresses it.** The button shrinks by a few pixels on a
//!   critically damped spring and comes back up on an underdamped one, so a
//!   tap lands with a small, physical bounce.
//!
//! Virtualised lists reuse widget state by *position*, so as a list scrolls a
//! button can find itself showing a different item. Give such buttons a
//! [`Button::key`] (the item's id) and they snap instead of crossfading from
//! the previous item's look.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{overlay, Clipboard, Shell};
use iced::event::{self, Event};
use iced::{mouse, touch, window};
use iced::{
    Background, Color, Element, Length, Padding, Rectangle, Size, Theme, Transformation, Vector,
};

use crate::ui::motion::{self, Animated, Spring};

pub use iced::widget::button::{primary, text, Status, Style};

type Renderer = iced::Renderer;

/// Time constants (seconds) for colour transitions — see the module docs.
const TAU_IN: f32 = 0.035;
const TAU_OUT: f32 = 0.12;
const TAU_SWAP: f32 = 0.07;
/// Below this per-channel difference a colour transition counts as done.
const SETTLED: f32 = 0.002;

/// How far a press pushes the button in, in pixels across its larger side,
/// and the cap on that as a fraction (so small icon buttons don't collapse).
const PRESS_DEPTH_PX: f32 = 3.0;
const PRESS_MAX_FRACTION: f32 = 0.05;

/// Same signature as `iced::widget::button`.
pub fn button<'a, Message>(content: impl Into<Element<'a, Message>>) -> Button<'a, Message> {
    Button::new(content)
}

type StyleFn<'a> = Box<dyn Fn(&Theme, Status) -> Style + 'a>;

pub struct Button<'a, Message> {
    content: Element<'a, Message>,
    on_press: Option<Message>,
    width: Length,
    height: Length,
    padding: Padding,
    clip: bool,
    style: StyleFn<'a>,
    key: Option<u64>,
}

/// iced's own default.
const DEFAULT_PADDING: Padding = Padding { top: 5.0, bottom: 5.0, right: 10.0, left: 10.0 };

impl<'a, Message> Button<'a, Message> {
    pub fn new(content: impl Into<Element<'a, Message>>) -> Self {
        let content = content.into();
        let size = content.as_widget().size_hint();
        Self {
            content,
            on_press: None,
            width: size.width.fluid(),
            height: size.height.fluid(),
            padding: DEFAULT_PADDING,
            clip: false,
            style: Box::new(primary),
            key: None,
        }
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    pub fn padding<P: Into<Padding>>(mut self, padding: P) -> Self {
        self.padding = padding.into();
        self
    }

    /// Without one the button is disabled, exactly as in iced.
    pub fn on_press(mut self, on_press: Message) -> Self {
        self.on_press = Some(on_press);
        self
    }

    pub fn on_press_maybe(mut self, on_press: Option<Message>) -> Self {
        self.on_press = on_press;
        self
    }

    pub fn clip(mut self, clip: bool) -> Self {
        self.clip = clip;
        self
    }

    pub fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self {
        self.style = Box::new(style);
        self
    }

    /// Identity of what this button represents. When it changes the button
    /// shows its new look immediately instead of transitioning to it.
    pub fn key(mut self, key: u64) -> Self {
        self.key = Some(key);
        self
    }
}

/// How "engaged" a status is; moving up the ranks uses the fast attack,
/// moving down the slow release.
fn rank(status: Status) -> u8 {
    match status {
        Status::Active | Status::Disabled => 0,
        Status::Hovered => 1,
        Status::Pressed => 2,
    }
}

fn mix(a: Style, b: Style, t: f32) -> Style {
    Style {
        background: motion::mix_background(a.background, b.background, t),
        text_color: motion::lerp_color(a.text_color, b.text_color, t),
        border: motion::mix_border(a.border, b.border, t),
        shadow: motion::mix_shadow(a.shadow, b.shadow, t),
    }
}

/// Largest visible difference between two styles, for "are we there yet".
fn distance(a: &Style, b: &Style) -> f32 {
    let bg = |s: &Style| match s.background {
        Some(Background::Color(c)) => c,
        _ => Color::TRANSPARENT,
    };
    motion::color_distance(bg(a), bg(b))
        .max(motion::color_distance(a.text_color, b.text_color))
        .max(motion::color_distance(a.border.color, b.border.color))
        .max((a.border.width - b.border.width).abs())
        .max(motion::color_distance(a.shadow.color, b.shadow.color))
        .max((a.shadow.blur_radius - b.shadow.blur_radius).abs() / 10.0)
}

/// The transition state machine, separate from the widget plumbing so it can
/// be tested without a renderer.
#[derive(Debug, Clone)]
struct Transition {
    /// What is on screen. `None` until the first target arrives.
    shown: Option<Style>,
    target: Option<Style>,
    status: Status,
    tau: f32,
    /// 0 = at rest, 1 = fully pressed in.
    press: Animated,
    press_spring: Spring,
    clock: Option<Instant>,
}

impl Default for Transition {
    fn default() -> Self {
        Self {
            shown: None,
            target: None,
            status: Status::Active,
            tau: TAU_SWAP,
            press: Animated::new(0.0),
            press_spring: Spring::PRESS,
            clock: None,
        }
    }
}

impl Transition {
    /// Point the transition at `target` for `status`. Returns `true` if
    /// anything changed (the caller should request a frame).
    fn retarget(&mut self, status: Status, target: Style) -> bool {
        let changed_status = status != self.status;
        let changed_style = self.target != Some(target);
        if !changed_status && !changed_style {
            return false;
        }
        self.tau = match rank(status).cmp(&rank(self.status)) {
            std::cmp::Ordering::Greater => TAU_IN,
            std::cmp::Ordering::Less => TAU_OUT,
            std::cmp::Ordering::Equal => TAU_SWAP,
        };
        let pressed = status == Status::Pressed;
        if pressed != (self.press.target > 0.5) {
            self.press_spring = if pressed { Spring::PRESS } else { Spring::RELEASE };
            self.press.set(if pressed { 1.0 } else { 0.0 });
        }
        self.status = status;
        self.target = Some(target);
        if self.shown.is_none() {
            self.shown = Some(target);
        }
        true
    }

    /// Advance to `now`. Returns `true` while still animating.
    fn step(&mut self, now: Instant) -> bool {
        let dt = motion::frame_dt(self.clock, now);
        self.clock = Some(now);

        let colour_moving = match (self.shown, self.target) {
            (Some(shown), Some(target)) => {
                let next = mix(shown, target, motion::approach_factor(dt, self.tau));
                if distance(&next, &target) < SETTLED {
                    self.shown = Some(target);
                    false
                } else {
                    self.shown = Some(next);
                    true
                }
            }
            _ => false,
        };
        let press_moving = self.press.step(self.press_spring, dt);

        let moving = colour_moving || press_moving;
        if !moving {
            // Next run starts with dt = 0 rather than one huge step.
            self.clock = None;
        }
        moving
    }

    /// Jump to `target` with no transition (new identity).
    fn snap(&mut self) {
        *self = Self::default();
    }
}

struct State {
    is_pressed: bool,
    transition: Transition,
    key: Option<u64>,
    /// The theme from the last draw: `on_event` has to evaluate the style
    /// function to notice a style swap, but only `draw` is handed a theme.
    theme: RefCell<Option<Theme>>,
    /// Whether the last draw was given an unavailable cursor while the
    /// pointer was in fact over the window — i.e. a layer above (a modal's
    /// scrim) owns the pointer. `on_event` isn't told about occlusion, so
    /// without this, buttons under a modal would light up on hover.
    occluded: Cell<bool>,
}

impl<'a, Message: Clone + 'a> Button<'a, Message> {
    fn status(&self, state: &State, cursor: mouse::Cursor, bounds: Rectangle) -> Status {
        if self.on_press.is_none() {
            Status::Disabled
        } else if cursor.is_over(bounds) && !state.occluded.get() {
            if state.is_pressed {
                Status::Pressed
            } else {
                Status::Hovered
            }
        } else {
            Status::Active
        }
    }
}

impl<'a, Message: Clone + 'a> Widget<Message, Theme, Renderer> for Button<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            is_pressed: false,
            transition: Transition::default(),
            key: self.key,
            theme: RefCell::new(None),
            occluded: Cell::new(false),
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.key != self.key {
            state.key = self.key;
            state.is_pressed = false;
            state.transition.snap();
        }
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size { width: self.width, height: self.height }
    }

    fn layout(&self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::padded(limits, self.width, self.height, self.padding, |limits| {
            self.content.as_widget().layout(&mut tree.children[0], renderer, limits)
        })
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds(), &mut |operation| {
            self.content.as_widget().operate(
                &mut tree.children[0],
                layout.children().next().unwrap(),
                renderer,
                operation,
            );
        });
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
        let mut status = self.content.as_widget_mut().on_event(
            &mut tree.children[0],
            event.clone(),
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();

        if status == event::Status::Ignored {
            match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                | Event::Touch(touch::Event::FingerPressed { .. }) => {
                    if self.on_press.is_some() && cursor.is_over(bounds) {
                        state.is_pressed = true;
                        status = event::Status::Captured;
                    }
                }
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Touch(touch::Event::FingerLifted { .. }) => {
                    if let Some(on_press) = self.on_press.clone() {
                        if state.is_pressed {
                            state.is_pressed = false;
                            if cursor.is_over(bounds) {
                                shell.publish(on_press);
                            }
                            status = event::Status::Captured;
                        }
                    }
                }
                Event::Touch(touch::Event::FingerLost { .. }) => state.is_pressed = false,
                _ => {}
            }
        }

        // Animation. Only possible once a draw has supplied a theme.
        let theme = state.theme.borrow().clone();
        if let Some(theme) = theme {
            let button_status = self.status(state, cursor, bounds);
            let target = (self.style)(&theme, button_status);
            let changed = state.transition.retarget(button_status, target);
            let moving = match event {
                Event::Window(window::Event::RedrawRequested(now)) => state.transition.step(now),
                _ => changed,
            };
            if moving {
                shell.request_redraw(window::RedrawRequest::NextFrame);
            }
        }

        status
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        if state.theme.borrow().as_ref() != Some(theme) {
            *state.theme.borrow_mut() = Some(theme.clone());
        }
        let bounds = layout.bounds();
        // An unavailable cursor in draw, while on_event can see a real one,
        // means a layer above has claimed the pointer.
        state.occluded.set(cursor == mouse::Cursor::Unavailable);

        let style = state
            .transition
            .shown
            .unwrap_or_else(|| (self.style)(theme, self.status(state, cursor, bounds)));

        let depth = (PRESS_DEPTH_PX / bounds.width.max(bounds.height).max(1.0))
            .min(PRESS_MAX_FRACTION);
        let scale = 1.0 - depth * state.transition.press.value;

        let draw_body = |renderer: &mut Renderer| {
            if style.background.is_some() || style.border.width > 0.0 || style.shadow.color.a > 0.0
            {
                iced::advanced::Renderer::fill_quad(
                    renderer,
                    renderer::Quad { bounds, border: style.border, shadow: style.shadow },
                    style.background.unwrap_or(Background::Color(Color::TRANSPARENT)),
                );
            }
            let viewport = if self.clip {
                bounds.intersection(viewport).unwrap_or(*viewport)
            } else {
                *viewport
            };
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                &renderer::Style { text_color: style.text_color },
                layout.children().next().unwrap(),
                cursor,
                &viewport,
            );
        };

        if (scale - 1.0).abs() < 1e-4 {
            draw_body(renderer);
        } else {
            let c = bounds.center();
            let t = Transformation::translate(c.x, c.y)
                * Transformation::scale(scale)
                * Transformation::translate(-c.x, -c.y);
            iced::advanced::Renderer::with_transformation(renderer, t, draw_body);
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) && self.on_press.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
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

impl<'a, Message: Clone + 'a> From<Button<'a, Message>> for Element<'a, Message> {
    fn from(button: Button<'a, Message>) -> Self {
        Self::new(button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn style(grey: f32) -> Style {
        Style {
            background: Some(Background::Color(Color::from_rgb(grey, grey, grey))),
            text_color: Color::WHITE,
            ..Style::default()
        }
    }

    fn grey_of(s: &Style) -> f32 {
        match s.background {
            Some(Background::Color(c)) => c.r,
            _ => panic!("no background"),
        }
    }

    /// Run frames at 60fps for `ms`, returning whether it was still moving.
    fn run(t: &mut Transition, start: Instant, ms: u64) -> (bool, Instant) {
        let mut now = start;
        let mut moving = true;
        while now < start + Duration::from_millis(ms) {
            moving = t.step(now);
            now += Duration::from_micros(16_667);
        }
        (moving, now)
    }

    #[test]
    fn first_style_is_shown_immediately() {
        let mut t = Transition::default();
        t.retarget(Status::Active, style(0.2));
        assert_eq!(t.shown, Some(style(0.2)));
    }

    /// Hover arrives near-instantly and leaves slowly — the asymmetry is the
    /// point.
    #[test]
    fn hover_attacks_fast_and_releases_slowly() {
        let start = Instant::now();
        let mut t = Transition::default();
        t.retarget(Status::Active, style(0.2));
        t.retarget(Status::Hovered, style(0.4));
        let (_, now) = run(&mut t, start, 70);
        let attack = (grey_of(&t.shown.unwrap()) - 0.2) / 0.2;
        assert!(attack > 0.75, "hover only {attack:.2} of the way after 70ms");

        let (_, _) = run(&mut t, now, 300);
        assert_eq!(t.shown, Some(style(0.4)), "settles exactly");

        t.retarget(Status::Active, style(0.2));
        let later = now + Duration::from_millis(400);
        run(&mut t, later, 70);
        let release = (0.4 - grey_of(&t.shown.unwrap())) / 0.2;
        assert!(release < attack, "release ({release:.2}) should trail attack ({attack:.2})");
    }

    #[test]
    fn a_style_swap_at_the_same_status_crossfades_and_settles() {
        let start = Instant::now();
        let mut t = Transition::default();
        t.retarget(Status::Active, style(0.1));
        assert!(t.retarget(Status::Active, style(0.5)), "swap must request frames");
        run(&mut t, start, 30);
        let mid = grey_of(&t.shown.unwrap());
        assert!(mid > 0.1 && mid < 0.5, "should be mid-crossfade, was {mid}");
        let (moving, _) = run(&mut t, start + Duration::from_millis(50), 600);
        assert!(!moving);
        assert_eq!(t.shown, Some(style(0.5)));
    }

    #[test]
    fn pressing_depresses_and_releasing_springs_back() {
        let start = Instant::now();
        let mut t = Transition::default();
        t.retarget(Status::Hovered, style(0.3));
        t.retarget(Status::Pressed, style(0.2));
        let (_, now) = run(&mut t, start, 120);
        assert!(t.press.value > 0.9, "pressed only {}", t.press.value);

        t.retarget(Status::Hovered, style(0.3));
        let mut overshot = false;
        let mut at = now;
        for _ in 0..90 {
            t.step(at);
            overshot |= t.press.value < -0.01;
            at += Duration::from_micros(16_667);
        }
        assert!(overshot, "release should bounce slightly past rest");
        assert_eq!(t.press.value, 0.0);
    }

    #[test]
    fn nothing_to_do_requests_no_frames() {
        let mut t = Transition::default();
        t.retarget(Status::Active, style(0.2));
        assert!(!t.retarget(Status::Active, style(0.2)));
        assert!(!t.step(Instant::now()));
    }
}
