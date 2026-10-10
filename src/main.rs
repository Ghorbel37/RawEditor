// Tests build params up field by field (`let mut p = default(); p.x = …`),
// which reads better than one giant literal. Production code is still held to
// the lint by the non-test build.
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

mod app;
pub mod color;
mod core;
mod database;
mod debug;
mod gpu;
mod raw;
mod ui;

use iced::Font;

pub fn main() -> iced::Result {
    let _guard = core::logging::init();
    iced::application(
        app::state::RawEditor::title,
        app::update::update,
        app::view::view,
    )
    .subscription(app::state::RawEditor::subscription)
    .theme(app::view::theme)
    .font(ui::icons::ICON_FONT_BYTES)
    .default_font(Font::with_name("JetBrainsMono Nerd Font"))
    .window(iced::window::Settings {
        size: iced::Size::new(900.0, 350.0),
        min_size: Some(iced::Size::new(600.0, 350.0)),
        decorations: false,
        ..Default::default()
    })
    .centered()
    .run_with(app::state::RawEditor::new)
}
