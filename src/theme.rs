//! Внешний вид окна: цвета, шрифты, размеры.
//!
//! Палитра построена вокруг «сигнала»: бирюзовый — звук идёт как надо,
//! коралловый — включён лаг, янтарный — что-то требует внимания.

use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};

/// Фон окна.
pub const BG: Color32 = Color32::from_rgb(0x16, 0x1B, 0x26);
/// Узлы цепочки, плитки.
pub const PANEL: Color32 = Color32::from_rgb(0x1F, 0x26, 0x33);
/// Кнопки и поля.
pub const RAISED: Color32 = Color32::from_rgb(0x2A, 0x33, 0x45);
pub const RAISED_HOVER: Color32 = Color32::from_rgb(0x34, 0x3F, 0x55);
/// Рамки и разделители.
pub const LINE: Color32 = Color32::from_rgb(0x34, 0x3E, 0x52);
/// Утопленные области: дорожки индикаторов и ползунков.
pub const SUNKEN: Color32 = Color32::from_rgb(0x10, 0x14, 0x1C);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xEA, 0xF2);
pub const MUTED: Color32 = Color32::from_rgb(0x8A, 0x94, 0xA7);
/// Звук идёт как надо.
pub const SIGNAL: Color32 = Color32::from_rgb(0x3C, 0xC8, 0xB4);
/// Тусклый бирюзовый — фон выбранного.
pub const SIGNAL_DIM: Color32 = Color32::from_rgb(0x1C, 0x4A, 0x4A);
/// Лаг.
pub const LAG: Color32 = Color32::from_rgb(0xF0, 0x50, 0x6E);
/// Требует внимания.
pub const WARN: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);

/// Полужирное начертание (заголовки, главная кнопка).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Настраивает шрифты и цвета. Вызывается один раз при запуске.
pub fn apply(ctx: &egui::Context) {
    set_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        use TextStyle::*;
        style.text_styles.insert(Body, regular(15.0));
        style.text_styles.insert(Button, regular(15.0));
        style.text_styles.insert(Small, regular(12.5));
        style.text_styles.insert(Heading, semibold(19.0));
        style.text_styles.insert(Monospace, FontId::monospace(14.0));

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 8.0);
        s.button_padding = egui::vec2(12.0, 5.0);
        s.interact_size.y = 28.0;
        s.slider_width = 260.0;
        s.combo_width = 260.0;

        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = SUNKEN;
        v.faint_bg_color = Color32::from_rgb(0x1B, 0x21, 0x2D);
        v.hyperlink_color = SIGNAL;
        v.warn_fg_color = WARN;
        v.error_fg_color = LAG;
        v.slider_trailing_fill = true;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.selection.bg_fill = SIGNAL_DIM;
        v.selection.stroke = Stroke::new(1.0, SIGNAL);

        let radius = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = radius;

        w.inactive.bg_fill = RAISED;
        w.inactive.weak_bg_fill = RAISED;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = radius;

        w.hovered.bg_fill = RAISED_HOVER;
        w.hovered.weak_bg_fill = RAISED_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, MUTED);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = radius;

        w.active.bg_fill = Color32::from_rgb(0x3D, 0x49, 0x61);
        w.active.weak_bg_fill = Color32::from_rgb(0x3D, 0x49, 0x61);
        w.active.bg_stroke = Stroke::new(1.0, SIGNAL);
        w.active.fg_stroke = Stroke::new(2.0, TEXT);
        w.active.corner_radius = radius;

        w.open.bg_fill = RAISED_HOVER;
        w.open.weak_bg_fill = RAISED_HOVER;
        w.open.bg_stroke = Stroke::new(1.0, MUTED);
        w.open.fg_stroke = Stroke::new(1.0, TEXT);
        w.open.corner_radius = radius;
    });
}

/// Системный Segoe UI (есть в Windows 10/11). Если файлов нет — остаются шрифты egui.
fn set_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let read = |name: &str| std::fs::read(format!("C:\\Windows\\Fonts\\{name}")).ok();
    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();

    let regular = read("segoeui.ttf");
    let bold = read("seguisb.ttf").or_else(|| read("segoeuib.ttf"));

    if let Some(bytes) = regular {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe".into());
    }
    let mut semibold_family = Vec::new();
    if let Some(bytes) = bold {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold_family.push("segoe-semibold".to_string());
    }
    // Запасные шрифты (символы, эмодзи) — те же, что у обычного текста.
    semibold_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or(defaults));
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold_family);
    ctx.set_fonts(fonts);
}
