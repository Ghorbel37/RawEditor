//! Preferences: a side menu of pages (`state::PreferencesSection`) beside a
//! scrolling content pane.
//!
//! The card has a fixed size so switching pages never resizes it under the
//! pointer; the menu's highlight slides between rows (`indicator::vertical`)
//! and the page fades up through the dialog surface (`fade_through`).

use iced::font::{Font, Weight};
use iced::widget::{
    checkbox, column, container, horizontal_rule, row, scrollable, slider, stack, text,
    vertical_rule, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};

use crate::app::message::Message;
use crate::app::state::{PreferencesSection, RawEditor};
use crate::core::paths::AppFolder;
use crate::ui;
use crate::ui::icons::ICON_FONT;
use crate::ui::widgets::button;

const MENU_WIDTH: f32 = 168.0;
const MENU_ROW: f32 = 34.0;
const BODY_HEIGHT: f32 = 460.0;
const CARD_WIDTH: f32 = 700.0;

/// The content pane's scrollable. Reset to the top on every page change
/// (`handlers::window::handle_set_preferences_section`): iced matches widget
/// state by position, so without that a new page opens at the old one's
/// scroll offset.
pub fn content_scroll_id() -> scrollable::Id {
    scrollable::Id::new("preferences-content")
}

impl PreferencesSection {
    fn label(self) -> &'static str {
        match self {
            PreferencesSection::General => "General",
            PreferencesSection::Performance => "Performance",
            PreferencesSection::Memory => "Memory",
            PreferencesSection::Folders => "Folders",
            PreferencesSection::Licenses => "Licenses",
        }
    }

    /// Font Awesome codepoints in the embedded Nerd Font.
    fn icon(self) -> &'static str {
        match self {
            PreferencesSection::General => "\u{f013}",     // cog
            PreferencesSection::Performance => "\u{f0e4}", // tachometer
            PreferencesSection::Memory => "\u{f2db}",      // microchip
            PreferencesSection::Folders => ui::icons::FOLDER,
            PreferencesSection::Licenses => "\u{f24e}",    // balance scale
        }
    }
}

pub fn view_preferences_modal(editor: &RawEditor) -> Element<'_, Message> {
    let section = editor.preferences_section;

    let page = match section {
        PreferencesSection::General => general(editor),
        PreferencesSection::Performance => performance(editor),
        PreferencesSection::Memory => memory(editor),
        PreferencesSection::Folders => folders(),
        PreferencesSection::Licenses => licenses(),
    };
    let content = scrollable(container(page).padding([4, 18]).width(Length::Fill))
        .id(content_scroll_id())
        .width(Length::Fill)
        .height(Length::Fill);
    let content = crate::ui::widgets::transition::fade_through(
        section.index() as u64,
        ui::styles::modal_surface().background,
        content,
    );

    column![
        text("Preferences").size(18).font(bold()).style(|_| text::Style {
            color: Some(Color::from_rgb(0.85, 0.85, 0.85))
        }),
        rule(),
        row![menu(section), vertical_rule(1).style(rule_style), content]
            .spacing(0)
            .height(Length::Fixed(BODY_HEIGHT)),
        rule(),
        row![
            // Resets every page, not just the one showing — the label says so.
            button(text("Reset All to Defaults").size(13))
                .on_press(Message::ResetPreferences)
                .padding([8, 14])
                .style(ui::styles::NeutralButton::style),
            Space::with_width(Length::Fill),
            button(text("Close").size(13).align_x(iced::alignment::Horizontal::Center))
                .on_press(Message::CloseModal)
                .padding([8, 14])
                .width(Length::Fixed(110.0))
                .style(ui::styles::AccentButton::style),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(12)
    .width(CARD_WIDTH)
    .into()
}

/// The side menu: transparent row buttons over a highlight that slides
/// between them, as the title bar's tabs do.
fn menu(active: PreferencesSection) -> Element<'static, Message> {
    let rows = PreferencesSection::ALL.iter().fold(column![], |rows, &section| {
        let is_active = section == active;
        rows.push(
            button(
                container(
                    row![
                        text(section.icon())
                            .font(ICON_FONT)
                            .size(13)
                            .width(Length::Fixed(18.0))
                            .align_x(iced::alignment::Horizontal::Center),
                        text(section.label()).size(13),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .height(Length::Fill)
                .align_y(iced::alignment::Vertical::Center),
            )
            .width(Length::Fill)
            .height(Length::Fixed(MENU_ROW))
            .padding([0, 14])
            .style(move |t, s| ui::styles::TabButton { is_active }.style(t, s))
            .on_press(Message::SetPreferencesSection(section)),
        )
    });
    let highlight = crate::ui::widgets::indicator::indicator(
        PreferencesSection::ALL.len(),
        active.index(),
        MENU_ROW,
        crate::ui::widgets::indicator::Appearance {
            fill: Color::from_rgb(0.15, 0.15, 0.15),
            accent: ui::palette::accent(),
            accent_height: 2.0,
            accent_inset: 8.0,
        },
    )
    .vertical();

    container(stack![highlight, rows])
        .width(Length::Fixed(MENU_WIDTH))
        .height(Length::Fill)
        .padding([0, 8])
        .into()
}

// ── pages ───────────────────────────────────────────────────────────────────

fn general(editor: &RawEditor) -> Element<'_, Message> {
    column![
        heading("General"),
        group_label("Workflow"),
        column![
            checkbox("Auto-Advance on Rate/Flag", editor.auto_advance)
                .on_toggle(|_| Message::ToggleAutoAdvance)
                .size(16)
                .spacing(8)
                .text_size(13)
                .style(checkbox_style),
            hint("After rating or flagging an image, move on to the next one."),
        ]
        .spacing(4),
        // Animations are short and never block input, but some people find
        // motion uncomfortable; this lands every one on its end state.
        column![
            checkbox("Reduce motion", editor.reduce_motion)
                .on_toggle(|_| Message::ToggleReduceMotion)
                .size(16)
                .spacing(8)
                .text_size(13)
                .style(checkbox_style),
            hint("Transitions and animations jump straight to their end state."),
        ]
        .spacing(4),
    ]
    .spacing(12)
    .into()
}

fn performance(editor: &RawEditor) -> Element<'_, Message> {
    let used_mb = editor.raw_cache_bytes as f32 / (1024.0 * 1024.0);
    let budget_mb = editor.raw_preload_budget_mb as f32;

    column![
        heading("Performance"),
        readout(format!("RAW Cache Usage: {used_mb:.0} / {budget_mb:.0} MB")),
        group_label("Previews"),
        value_row("Memory Cache Size:", format!("{} Images", editor.cache_capacity)),
        slider(20.0..=500.0, editor.cache_capacity as f32, Message::SetCacheCapacity)
            .on_release(Message::SavePreferences)
            .step(10.0_f32)
            .style(ui::styles::ProSlider::style),
        hint("Controls how many high-res previews are kept in RAM."),
        value_row(
            "Preview Preload (Behind/Ahead):",
            format!("{}/{}", editor.preview_preload_behind, editor.preview_preload_ahead),
        ),
        labelled_slider(
            "Behind",
            slider(
                0.0..=crate::app::state::PREVIEW_PRELOAD_BEHIND_MAX as f32,
                editor.preview_preload_behind as f32,
                Message::SetPreviewPreloadBehind,
            )
            .on_release(Message::SavePreferences)
            .step(1.0_f32)
            .style(ui::styles::ProSlider::style),
        ),
        labelled_slider(
            "Ahead",
            slider(
                0.0..=crate::app::state::PREVIEW_PRELOAD_AHEAD_MAX as f32,
                editor.preview_preload_ahead as f32,
                Message::SetPreviewPreloadAhead,
            )
            .on_release(Message::SavePreferences)
            .step(1.0_f32)
            .style(ui::styles::ProSlider::style),
        ),
        hint("How many working previews stay queued around your current image."),
        group_label("RAW Files"),
        value_row("RAW Preload Budget:", format!("{} MB", editor.raw_preload_budget_mb)),
        slider(0.0..=4096.0, editor.raw_preload_budget_mb as f32, Message::SetRawPreloadBudget)
            .on_release(Message::SavePreferences)
            .step(64.0_f32)
            .style(ui::styles::ProSlider::style),
        hint("Develop RAW preloading budget. Set to 0 to disable RAW preloading."),
        value_row(
            "RAW Preload (Behind/Ahead):",
            format!("{}/{}", editor.raw_preload_behind, editor.raw_preload_ahead),
        ),
        labelled_slider(
            "Behind",
            slider(
                0.0..=crate::app::state::RAW_PRELOAD_BEHIND_MAX as f32,
                editor.raw_preload_behind as f32,
                Message::SetRawPreloadBehind,
            )
            .on_release(Message::SavePreferences)
            .step(1.0_f32)
            .style(ui::styles::ProSlider::style),
        ),
        labelled_slider(
            "Ahead",
            slider(
                0.0..=crate::app::state::RAW_PRELOAD_AHEAD_MAX as f32,
                editor.raw_preload_ahead as f32,
                Message::SetRawPreloadAhead,
            )
            .on_release(Message::SavePreferences)
            .step(1.0_f32)
            .style(ui::styles::ProSlider::style),
        ),
        hint("How many full RAWs are pre-decoded for faster Develop navigation."),
    ]
    .spacing(8)
    .into()
}

fn memory(editor: &RawEditor) -> Element<'_, Message> {
    // Live readout. On a platform where available memory can't be read, say
    // so rather than showing a number that isn't there.
    let available = match crate::core::memory::available_bytes() {
        Some(b) => format!("System memory available: {} MB", crate::core::memory::as_mb(b)),
        None => "System memory unavailable on this platform — check disabled".to_string(),
    };

    column![
        heading("Memory"),
        group_label("Memory Safety"),
        value_row(
            "Keep RAM Free:",
            if editor.min_free_ram_mb == 0 {
                "Off".to_string()
            } else {
                format!("{} MB", editor.min_free_ram_mb)
            },
        ),
        slider(
            0.0..=crate::app::state::MIN_FREE_RAM_MB_MAX as f32,
            editor.min_free_ram_mb as f32,
            Message::SetMinFreeRam,
        )
        .on_release(Message::SavePreferences)
        .step(256.0_f32)
        .style(ui::styles::ProSlider::style),
        readout(available),
        hint("Batch export pauses if free memory drops below this, after first dropping its own caches. 0 disables the check."),
    ]
    .spacing(8)
    .into()
}

fn folders() -> Element<'static, Message> {
    column![
        heading("Folders"),
        hint("RawEditor keeps its own files in two folders. Your RAW files stay where they are and are never copied into either."),
        folder_card(
            "Library & Settings",
            AppFolder::Data,
            &[
                ("raw_editor.db", "The catalogue: imported images, ratings, flags and edits"),
                ("settings.json", "These preferences"),
                ("profiles/", "DCP camera profiles — put Adobe .dcp files here"),
                ("lensfun/", "Extra lensfun lens-profile XML files"),
                ("logs/", "Diagnostic logs"),
            ],
            "Back this folder up to keep your edits.",
        ),
        folder_card(
            "Cache",
            AppFolder::Cache,
            &[
                ("thumb/", "Library grid thumbnails"),
                ("instant/", "Previews for fast full-screen browsing"),
                ("working/", "Develop previews"),
                ("edited/", "Thumbnails of your images as edited"),
            ],
            "Safe to delete while RawEditor is closed: anything missing is rebuilt.",
        ),
    ]
    .spacing(12)
    .into()
}

fn folder_card(
    title: &'static str,
    folder: AppFolder,
    contents: &'static [(&'static str, &'static str)],
    note: &'static str,
) -> Element<'static, Message> {
    let path = folder.path();
    let path_text = path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "Not available on this platform".to_string());

    let listing = contents.iter().fold(column![].spacing(4), |list, &(name, what)| {
        list.push(
            row![
                text(name).size(12).width(Length::Fixed(120.0)).style(|_| text::Style {
                    color: Some(Color::from_rgb(0.8, 0.8, 0.8))
                }),
                text(what).size(11).style(|_| text::Style {
                    color: Some(Color::from_rgb(0.5, 0.5, 0.5))
                }),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
    });

    let open = button(
        row![text(ui::icons::FOLDER_OPEN).font(ICON_FONT).size(13), text("Open Folder").size(13)]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .on_press_maybe(path.is_some().then_some(Message::OpenAppFolder(folder)))
    .padding([6, 12])
    .style(ui::styles::NeutralButton::style);

    card(
        column![
            row![
                text(title).size(14).font(bold()).style(|_| text::Style {
                    color: Some(Color::from_rgb(0.88, 0.88, 0.88))
                }),
                Space::with_width(Length::Fill),
                open,
            ]
            .align_y(Alignment::Center),
            text(path_text).size(12).style(|_| text::Style {
                color: Some(ui::palette::accent_text())
            }),
            listing,
            hint(note),
        ]
        .spacing(10),
    )
}

// ── licenses ────────────────────────────────────────────────────────────────

/// One credited work. `crates` ties library entries to `Cargo.toml`, so a
/// test can insist every direct dependency is credited.
struct Credit {
    name: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    crates: &'static [&'static str],
    license: &'static str,
    purpose: &'static str,
    url: Option<&'static str>,
}

const fn credit(
    name: &'static str,
    crates: &'static [&'static str],
    license: &'static str,
    purpose: &'static str,
    url: &'static str,
) -> Credit {
    Credit { name, crates, license, purpose, url: Some(url) }
}

/// Compiled or embedded into the binary, beyond the crates below.
const BUNDLED: &[Credit] = &[
    credit(
        "JetBrains Mono Nerd Font",
        &[],
        "OFL-1.1",
        "Interface and icon font",
        "https://github.com/ryanoasis/nerd-fonts",
    ),
    credit(
        "SQLite",
        &[],
        "Public domain",
        "Catalogue database engine, compiled in by rusqlite",
        "https://sqlite.org/copyright.html",
    ),
];

/// Direct dependencies (and wgpu, which iced_wgpu brings in but which does
/// the app's heaviest work), alphabetical.
const LIBRARIES: &[Credit] = &[
    credit("bytemuck", &["bytemuck"], "Zlib OR Apache-2.0 OR MIT", "GPU buffer layout", "https://github.com/Lokathor/bytemuck"),
    credit("cgmath", &["cgmath"], "Apache-2.0", "Matrix maths", "https://github.com/rustgd/cgmath"),
    credit("chrono", &["chrono"], "MIT OR Apache-2.0", "Dates and times", "https://github.com/chronotope/chrono"),
    credit("dirs", &["dirs"], "MIT OR Apache-2.0", "Platform folder locations", "https://github.com/soc/dirs-rs"),
    credit("half", &["half"], "MIT OR Apache-2.0", "16-bit floats", "https://github.com/VoidStarKat/half-rs"),
    credit("iced", &["iced", "iced_wgpu"], "MIT", "GUI toolkit", "https://github.com/iced-rs/iced"),
    credit("iced_aw", &["iced_aw"], "MIT", "Menu bar", "https://github.com/iced-rs/iced_aw"),
    credit("image · png", &["image", "png"], "MIT OR Apache-2.0", "Image decoding, PNG export", "https://github.com/image-rs/image"),
    credit("jpeg-encoder", &["jpeg-encoder"], "(MIT OR Apache-2.0) AND IJG", "JPEG export", "https://github.com/vstroebel/jpeg-encoder"),
    credit("kamadak-exif", &["kamadak-exif"], "BSD-2-Clause", "EXIF metadata", "https://github.com/kamadak/exif-rs"),
    credit("lru", &["lru"], "MIT", "Preview cache", "https://github.com/jeromefroe/lru-rs"),
    credit("rawler", &["rawler"], "LGPL-2.1", "RAW decoding, from the dnglab project", "https://github.com/dnglab/dnglab"),
    credit("rfd", &["rfd"], "MIT", "Native file dialogs", "https://github.com/PolyMeilex/rfd"),
    credit("roxmltree", &["roxmltree"], "MIT OR Apache-2.0", "lensfun XML parsing", "https://github.com/RazrFalcon/roxmltree"),
    credit("rusqlite", &["rusqlite"], "MIT", "SQLite bindings", "https://github.com/rusqlite/rusqlite"),
    credit("serde · serde_json", &["serde", "serde_json"], "MIT OR Apache-2.0", "Settings and edit storage", "https://github.com/serde-rs/serde"),
    credit("thiserror", &["thiserror"], "MIT OR Apache-2.0", "Error types", "https://github.com/dtolnay/thiserror"),
    credit("tiff", &["tiff"], "MIT", "TIFF export, DCP parsing", "https://github.com/image-rs/image-tiff"),
    credit("tokio", &["tokio"], "MIT", "Background work", "https://github.com/tokio-rs/tokio"),
    credit("tracing", &["tracing", "tracing-subscriber", "tracing-appender"], "MIT", "Logging", "https://github.com/tokio-rs/tracing"),
    credit("trash", &["trash"], "MIT", "Moving files to the system trash", "https://github.com/ArturKovacs/trash"),
    credit("walkdir", &["walkdir"], "Unlicense OR MIT", "Folder import", "https://github.com/BurntSushi/walkdir"),
    credit("wgpu", &[], "MIT OR Apache-2.0", "GPU rendering", "https://github.com/gfx-rs/wgpu"),
    credit("zune-jpeg", &["zune-jpeg", "zune-core"], "MIT OR Apache-2.0 OR Zlib", "Fast JPEG decoding (fast-jpeg builds)", "https://github.com/etemesi254/zune-image"),
];

/// Read at runtime when the user has them; never shipped with RawEditor.
const EXTERNAL: &[Credit] = &[
    credit(
        "Lensfun database",
        &[],
        "CC BY-SA 3.0",
        "Lens profiles, read from your system or the lensfun folder",
        "https://lensfun.github.io",
    ),
    Credit {
        name: "Adobe DCP profiles",
        crates: &[],
        license: "Adobe's terms",
        purpose: "Camera profiles you supply in the profiles folder",
        url: None,
    },
];

const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

fn licenses() -> Element<'static, Message> {
    let list = |credits: &'static [Credit]| {
        credits.iter().fold(column![].spacing(2), |list, c| list.push(credit_row(c)))
    };

    column![
        heading("Licenses"),
        row![
            text(format!("RawEditor {}", env!("CARGO_PKG_VERSION"))).size(14).font(bold()).style(
                |_| text::Style { color: Some(Color::from_rgb(0.88, 0.88, 0.88)) }
            ),
            Space::with_width(Length::Fill),
            text("MIT").size(11).style(|_| text::Style { color: Some(ui::palette::accent_text()) }),
        ]
        .align_y(Alignment::Center),
        card(text(LICENSE_TEXT.trim()).size(11).style(|_| text::Style {
            color: Some(Color::from_rgb(0.6, 0.6, 0.6))
        })),
        group_label("Bundled"),
        list(BUNDLED),
        group_label("Libraries"),
        list(LIBRARIES),
        hint("These depend on several hundred further crates, almost all under MIT, Apache-2.0, BSD or Zlib terms; each one's license is listed on crates.io."),
        // The IJG license asks for exactly this sentence wherever only the
        // executable is distributed (jpeg-encoder ports libjpeg code).
        hint("This software is based in part on the work of the Independent JPEG Group."),
        group_label("Used if installed"),
        list(EXTERNAL),
        hint("Neither is bundled: RawEditor only reads them from your disk."),
    ]
    .spacing(8)
    .into()
}

/// Name and purpose on the left, license on the right; the whole row opens
/// the project page when there is one.
fn credit_row(c: &'static Credit) -> Element<'static, Message> {
    let body = row![
        column![
            text(c.name).size(13).font(bold()).style(|_| text::Style {
                color: Some(Color::from_rgb(0.85, 0.85, 0.85))
            }),
            text(c.purpose).size(11).style(|_| text::Style {
                color: Some(Color::from_rgb(0.5, 0.5, 0.5))
            }),
        ]
        .spacing(2)
        .width(Length::Fill),
        text(c.license).size(11).style(|_| text::Style {
            color: Some(ui::palette::accent_text())
        }),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    match c.url {
        Some(url) => button(body)
            .width(Length::Fill)
            .padding([6, 8])
            .style(|t, s| ui::styles::TabButton { is_active: false }.style(t, s))
            .on_press(Message::OpenLink(url))
            .into(),
        None => container(body).padding([6, 8]).into(),
    }
}

// ── pieces ──────────────────────────────────────────────────────────────────

fn bold() -> Font {
    Font {
        weight: Weight::Bold,
        ..Default::default()
    }
}

fn heading(label: &str) -> Element<'_, Message> {
    text(label)
        .size(16)
        .font(bold())
        .style(|_| text::Style { color: Some(Color::from_rgb(0.9, 0.9, 0.9)) })
        .into()
}

fn group_label(label: &str) -> Element<'_, Message> {
    container(text(label).size(12).font(bold()).style(|_| text::Style {
        color: Some(Color::from_rgb(0.5, 0.5, 0.5))
    }))
    .padding(iced::Padding { top: 8.0, ..Default::default() })
    .into()
}

fn hint(s: &str) -> Element<'_, Message> {
    text(s)
        .size(11)
        .style(|_| text::Style { color: Some(Color::from_rgb(0.45, 0.45, 0.45)) })
        .into()
}

fn readout(s: String) -> Element<'static, Message> {
    text(s)
        .size(12)
        .style(|_| text::Style { color: Some(Color::from_rgb(0.55, 0.75, 0.55)) })
        .into()
}

fn value_row(label: &str, value: String) -> Element<'_, Message> {
    row![
        text(label).size(13).style(|_| text::Style {
            color: Some(Color::from_rgb(0.7, 0.7, 0.7))
        }),
        text(value).size(13).font(bold()).style(|_| text::Style {
            color: Some(Color::from_rgb(0.85, 0.85, 0.85))
        }),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn labelled_slider<'a>(
    label: &'a str,
    slider: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        text(label).size(11).width(Length::Fixed(52.0)).style(|_| text::Style {
            color: Some(Color::from_rgb(0.5, 0.5, 0.5))
        }),
        slider.into(),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(12)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgb(0.13, 0.13, 0.13))),
            border: Border {
                color: Color::from_rgb(0.2, 0.2, 0.2),
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn rule() -> Element<'static, Message> {
    horizontal_rule(1.0).style(rule_style).into()
}

fn rule_style(_theme: &Theme) -> iced::widget::rule::Style {
    iced::widget::rule::Style {
        color: Color::from_rgb(0.3, 0.3, 0.3),
        width: 1,
        radius: 0.0.into(),
        fill_mode: iced::widget::rule::FillMode::Full,
    }
}

fn checkbox_style(_theme: &Theme, _status: checkbox::Status) -> checkbox::Style {
    checkbox::Style {
        background: Background::Color(Color::from_rgb(0.2, 0.2, 0.2)),
        icon_color: Color::from_rgb(0.85, 0.85, 0.85),
        border: Border {
            color: Color::from_rgb(0.4, 0.4, 0.4),
            width: 1.0,
            radius: 3.0.into(),
        },
        text_color: Some(Color::from_rgb(0.7, 0.7, 0.7)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `[dependencies]` entry in Cargo.toml must be credited, so adding
    /// a crate without a Licenses line fails here rather than going unnoticed.
    #[test]
    fn every_direct_dependency_is_credited() {
        let manifest = include_str!("../../../Cargo.toml");
        let deps = manifest
            .split("\n[dependencies]\n")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("Cargo.toml has a [dependencies] table");
        let credited: Vec<&str> =
            LIBRARIES.iter().flat_map(|c| c.crates.iter().copied()).collect();

        let mut missing = Vec::new();
        for line in deps.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, _)) = line.split_once('=') else { continue };
            let name = name.trim();
            if !credited.contains(&name) {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "not credited in Preferences › Licenses: {missing:?}");
    }

    #[test]
    fn the_menu_lists_every_section_once_in_order() {
        for (i, s) in PreferencesSection::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
            assert!(!s.label().is_empty());
        }
    }
}
