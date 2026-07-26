// SPDX-License-Identifier: MIT OR Apache-2.0
//! The visual theme. Two palettes, chosen at startup (`--theme`):
//!
//! * `default` — iOS-inspired dark: blue accent, card-based layout.
//! * `pdp` — PDP-11/70 front panel: near-black panel behind a white bezel,
//!   magenta/purple accents, and red "LED" push buttons.
//!
//! Everything visual routes through the active [`Palette`] so the look
//! stays consistent across panels.

use std::sync::OnceLock;

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Sense, Shadow,
    Stroke, TextStyle, Ui,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeKind {
    #[default]
    Default,
    Pdp,
}

impl std::str::FromStr for ThemeKind {
    type Err = String;
    fn from_str(s: &str) -> Result<ThemeKind, String> {
        match s.to_ascii_lowercase().as_str() {
            "default" | "blue" => Ok(ThemeKind::Default),
            "pdp" | "pdp11" => Ok(ThemeKind::Pdp),
            _ => Err(format!("unknown theme {s:?}: expected default | pdp")),
        }
    }
}

pub struct Palette {
    pub bg: Color32,
    pub card: Color32,
    pub card_inset: Color32,
    /// Card and window border ("bezel" in the PDP theme).
    pub bezel: Color32,
    /// Primary accent: progress bars, selection, links, card-map occupancy.
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Push-button faces (separate from the accent so PDP buttons can be
    /// red LEDs while the accent stays magenta).
    pub button: Color32,
    pub button_hover: Color32,
    pub button_active: Color32,
    pub button_stroke: Color32,
    pub green: Color32,
    pub orange: Color32,
    pub red: Color32,
    pub teal: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub stripe: Color32,
    pub inset_bg: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn default_palette() -> Palette {
    Palette {
        bg: rgb(0x17, 0x19, 0x1f),
        card: rgb(0x20, 0x23, 0x2b),
        card_inset: rgb(0x28, 0x2b, 0x34),
        bezel: Color32::from_white_alpha(8),
        accent: rgb(0x0a, 0x84, 0xff),
        accent_hover: rgb(0x36, 0x99, 0xff),
        button: rgb(0x0a, 0x84, 0xff),
        button_hover: rgb(0x36, 0x99, 0xff),
        button_active: rgb(0x06, 0x62, 0xc4),
        button_stroke: Color32::TRANSPARENT,
        green: rgb(0x30, 0xd1, 0x58),
        orange: rgb(0xff, 0x9f, 0x0a),
        red: rgb(0xff, 0x45, 0x3a),
        teal: rgb(0x64, 0xd2, 0xff),
        text: rgb(0xf2, 0xf2, 0xf7),
        text_dim: rgb(0x8e, 0x8e, 0x93),
        stripe: rgb(0x27, 0x2a, 0x33),
        inset_bg: rgb(0x1b, 0x1d, 0x24),
    }
}

/// PDP-11/70 front panel: the magenta/purple rocker-switch colors as the
/// accent, red illuminated push buttons, off-white bezel lines.
fn pdp_palette() -> Palette {
    Palette {
        bg: rgb(0x0c, 0x0b, 0x0e),
        card: rgb(0x18, 0x16, 0x1a),
        card_inset: rgb(0x22, 0x1f, 0x24),
        bezel: rgb(0xd8, 0xd2, 0xc6),  // off-white panel bezel
        accent: rgb(0xc2, 0x4b, 0x8e), // 11/70 magenta
        accent_hover: rgb(0xd9, 0x6d, 0xa9),
        button: rgb(0x58, 0x10, 0x10), // dark red LED, unlit
        button_hover: rgb(0x8a, 0x16, 0x12),
        button_active: rgb(0xb8, 0x1d, 0x16), // pressed = lit
        button_stroke: rgb(0xff, 0x5a, 0x4a),
        green: rgb(0x30, 0xd1, 0x58),
        orange: rgb(0xff, 0x9f, 0x0a),
        red: rgb(0xff, 0x45, 0x3a),
        teal: rgb(0x64, 0xd2, 0xff),
        text: rgb(0xf4, 0xf0, 0xe8),
        text_dim: rgb(0x9a, 0x93, 0x88),
        stripe: rgb(0x20, 0x1d, 0x22),
        inset_bg: rgb(0x14, 0x12, 0x15),
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Choose the palette; call once before any UI is built. Later calls are
/// ignored (the palette is process-wide).
pub fn init(kind: ThemeKind) {
    let _ = PALETTE.set(match kind {
        ThemeKind::Default => default_palette(),
        ThemeKind::Pdp => pdp_palette(),
    });
}

/// The active palette (default theme if `init` was never called).
pub fn p() -> &'static Palette {
    PALETTE.get_or_init(default_palette)
}

/// The log pane's phosphor-CRT face: VT323 (OFL, bundled in assets/fonts —
/// a faithful DEC VT320 terminal font), with egui's defaults as glyph
/// fallback.
pub const PHOSPHOR_FAMILY: &str = "phosphor";
/// Classic P1 green phosphor.
pub const PHOSPHOR_GREEN: Color32 = Color32::from_rgb(0x3d, 0xff, 0x66);
/// The "tube" behind the log: near-black with a faint green cast.
pub const PHOSPHOR_BG: Color32 = Color32::from_rgb(0x06, 0x0e, 0x08);

pub fn phosphor_font() -> FontId {
    FontId::new(16.0, FontFamily::Name(PHOSPHOR_FAMILY.into()))
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "vt323".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/VT323-Regular.ttf"
        ))),
    );
    // VT323 first, then the stock fonts so uncovered glyphs still render.
    let mut family: Vec<String> = vec!["vt323".to_owned()];
    if let Some(mono) = fonts.families.get(&FontFamily::Monospace) {
        family.extend(mono.iter().cloned());
    }
    fonts
        .families
        .insert(FontFamily::Name(PHOSPHOR_FAMILY.into()), family);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    // Pin dark mode: egui otherwise follows the system theme, and on a
    // light-mode host it would lay light-theme chrome (black text!) over
    // our dark cards.
    ctx.set_theme(egui::ThemePreference::Dark);
    install_fonts(ctx);
    let pal = p();
    let mut style = (*ctx.style()).clone();

    // Type hierarchy: larger headings, comfortable body.
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(19.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(14.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(12.5, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );

    // Airy spacing.
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 6.0);
    style.spacing.interact_size.y = 26.0;

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = pal.bg;
    v.window_fill = pal.card;
    v.extreme_bg_color = pal.inset_bg;
    v.faint_bg_color = pal.stripe; // striped rows
    v.window_corner_radius = CornerRadius::same(14);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.window_stroke = Stroke::new(1.0_f32, pal.bezel);
    v.override_text_color = Some(pal.text);

    // Buttons and interactive widgets: rounded, LED-red under the PDP theme.
    let radius = CornerRadius::same(9);
    v.widgets.inactive.weak_bg_fill = pal.button;
    v.widgets.inactive.bg_fill = pal.button;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal.text);
    v.widgets.inactive.bg_stroke = if pal.button_stroke == Color32::TRANSPARENT {
        Stroke::NONE
    } else {
        Stroke::new(1.0_f32, pal.button_stroke)
    };
    v.widgets.hovered.weak_bg_fill = pal.button_hover;
    v.widgets.hovered.bg_fill = pal.button_hover;
    v.widgets.hovered.corner_radius = radius;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, pal.text);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(30));
    v.widgets.active.weak_bg_fill = pal.button_active;
    v.widgets.active.bg_fill = pal.button_active;
    v.widgets.active.corner_radius = radius;
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, pal.text);
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal.text);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(12));
    v.widgets.open.corner_radius = radius;
    v.widgets.open.bg_fill = pal.card_inset;
    v.widgets.open.weak_bg_fill = pal.card_inset;

    // Progress bar fill, selection highlights, and inline links.
    v.selection.bg_fill = pal.accent;
    v.selection.stroke = Stroke::new(1.0_f32, pal.text);
    v.hyperlink_color = pal.accent_hover;

    ctx.set_style(style);
}

/// A rounded, softly shadowed content card ("module" behind the bezel).
pub fn card() -> egui::Frame {
    let pal = p();
    egui::Frame::new()
        .fill(pal.card)
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(14, 12))
        .shadow(Shadow {
            offset: [0, 3],
            blur: 12,
            spread: 0,
            color: Color32::from_black_alpha(70),
        })
        .stroke(Stroke::new(1.0_f32, pal.bezel))
}

/// A capsule status badge: tinted background, colored text.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let bg = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 36);
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).size(12.0).strong());
        });
}

/// iOS-style animated toggle switch (the canonical egui custom widget).
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let desired_size = egui::vec2(44.0, 24.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let radius = 0.5 * rect.height();
        let off_color = Color32::from_rgb(0x3a, 0x3d, 0x46);
        let track = lerp_color(off_color, p().green, how_on);
        ui.painter().rect_filled(rect, radius, track);
        let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter().circle(
            egui::pos2(knob_x, rect.center().y),
            radius - 2.0,
            Color32::WHITE,
            Stroke::NONE,
        );
    }
    response
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| -> u8 { (x as f32 + (y as f32 - x as f32) * t) as u8 };
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// One settings row: label + explanation on the left, a toggle on the right.
/// Returns true when the value changed.
pub fn setting_row(ui: &mut Ui, label: &str, detail: &str, value: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(250.0);
            ui.label(RichText::new(label).color(p().text));
            if !detail.is_empty() {
                ui.label(RichText::new(detail).color(p().text_dim).size(11.0));
            }
        });
        changed = toggle(ui, value).changed();
    });
    ui.add_space(2.0);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_kind_parses() {
        assert_eq!("default".parse::<ThemeKind>().unwrap(), ThemeKind::Default);
        assert_eq!("PDP".parse::<ThemeKind>().unwrap(), ThemeKind::Pdp);
        assert_eq!("pdp11".parse::<ThemeKind>().unwrap(), ThemeKind::Pdp);
        assert!("solarized".parse::<ThemeKind>().is_err());
    }

    #[test]
    fn palettes_are_distinct() {
        let d = default_palette();
        let pdp = pdp_palette();
        assert_ne!(d.button, pdp.button);
        assert_ne!(d.accent, pdp.accent);
        // Semantic state colors stay identical across themes.
        assert_eq!(d.green, pdp.green);
        assert_eq!(d.red, pdp.red);
    }
}
