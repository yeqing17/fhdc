//! 视觉主题:仿照 fadb 的排版与配色,主色换成鸿蒙蓝。

use std::sync::Arc;

use eframe::egui;
use eframe::egui::Color32;
use eframe::egui::Context;
use eframe::egui::CornerRadius;
use eframe::egui::FontData;
use eframe::egui::FontDefinitions;
use eframe::egui::FontFamily;
use eframe::egui::Stroke;
use eframe::egui::Theme;
use eframe::egui::ThemePreference;
use eframe::egui::Visuals;

/// 主色:鸿蒙蓝。
pub const ACCENT: Color32 = Color32::from_rgb(31, 111, 235);

/// 面板之外的补充调色(卡片、气泡、危险区)。
pub struct Palette {
    pub central_fill: Color32,
    pub chip_fill: Color32,
    pub danger_fill: Color32,
    pub danger_stroke: Color32,
    pub terminal_bg: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub error: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            central_fill: Color32::from_rgb(21, 22, 26),
            chip_fill: Color32::from_rgb(42, 45, 53),
            danger_fill: Color32::from_rgb(62, 28, 32),
            danger_stroke: Color32::from_rgb(150, 66, 72),
            terminal_bg: Color32::from_rgb(14, 17, 22),
            ok: Color32::from_rgb(134, 239, 172),
            warn: Color32::from_rgb(250, 204, 21),
            error: Color32::from_rgb(248, 113, 113),
        }
    } else {
        Palette {
            central_fill: Color32::from_rgb(244, 245, 248),
            chip_fill: Color32::from_rgb(232, 234, 241),
            danger_fill: Color32::from_rgb(253, 235, 236),
            danger_stroke: Color32::from_rgb(226, 134, 140),
            terminal_bg: Color32::from_rgb(248, 250, 252),
            ok: Color32::from_rgb(22, 163, 74),
            warn: Color32::from_rgb(217, 119, 6),
            error: Color32::from_rgb(220, 38, 38),
        }
    }
}

/// 字体、主题槽与控件样式。中文必须进首位 Proportional,否则基线漂移。
pub fn configure(context: &Context) {
    if let Some(bytes) = load_cjk_font() {
        let mut fonts = FontDefinitions::default();
        fonts
            .font_data
            .insert("fhdc-cjk".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "fhdc-cjk".into());
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push("fhdc-cjk".into());
        context.set_fonts(fonts);
    }

    // 先钉住 Dark,再对两个槽位分别上样式,避免系统浅色时样式落错槽。
    context.set_theme(ThemePreference::Dark);
    context.set_visuals_of(Theme::Dark, build_visuals(true));
    context.set_visuals_of(Theme::Light, build_visuals(false));
    context.style_mut_of(Theme::Dark, apply_style);
    context.style_mut_of(Theme::Light, apply_style);
}

fn apply_style(style: &mut egui::Style) {
    use egui::TextStyle;
    style.spacing.item_spacing = [8.0, 8.0].into();
    style.spacing.button_padding = [10.0, 4.0].into();
    style.spacing.window_margin = 12.0.into();
    style
        .text_styles
        .get_mut(&TextStyle::Small)
        .expect("small style")
        .size = 11.5;
    style
        .text_styles
        .get_mut(&TextStyle::Body)
        .expect("body style")
        .size = 13.5;
    style
        .text_styles
        .get_mut(&TextStyle::Button)
        .expect("button style")
        .size = 13.0;
    style
        .text_styles
        .get_mut(&TextStyle::Heading)
        .expect("heading style")
        .size = 19.0;
    style
        .text_styles
        .get_mut(&TextStyle::Monospace)
        .expect("monospace style")
        .size = 12.5;
    style.visuals.indent_has_left_vline = false;
}

fn build_visuals(dark: bool) -> Visuals {
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.window_stroke = Stroke::new(1.0, visuals.weak_text_color());
    if dark {
        visuals.panel_fill = Color32::from_rgb(26, 27, 32);
        visuals.window_fill = Color32::from_rgb(30, 31, 37);
        visuals.extreme_bg_color = Color32::from_rgb(16, 17, 21);
        visuals.faint_bg_color = Color32::from_rgb(34, 36, 43);
        visuals.code_bg_color = Color32::from_rgb(38, 40, 48);
        visuals.selection.bg_fill = Color32::from_rgba_premultiplied(142, 148, 160, 85);
    } else {
        visuals.panel_fill = Color32::from_rgb(238, 240, 244);
        visuals.window_fill = Color32::from_rgb(252, 252, 254);
        visuals.extreme_bg_color = Color32::WHITE;
        visuals.faint_bg_color = Color32::from_rgb(230, 233, 239);
        visuals.code_bg_color = Color32::from_rgb(236, 238, 243);
        visuals.selection.bg_fill = Color32::from_rgba_premultiplied(125, 135, 155, 75);
    }
    visuals.hyperlink_color = ACCENT;
    visuals.weak_text_alpha = 0.55;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    visuals.widgets.active.corner_radius = CornerRadius::same(6);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(8);
    // 幽灵按钮:默认透明,悬停才出现底色。
    visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    visuals.widgets.hovered.weak_bg_fill = if dark {
        Color32::from_rgb(44, 46, 55)
    } else {
        Color32::from_rgb(223, 226, 233)
    };
    visuals.widgets.active.weak_bg_fill = if dark {
        Color32::from_rgb(52, 55, 66)
    } else {
        Color32::from_rgb(212, 216, 226)
    };
    visuals.widgets.active.bg_stroke = Stroke::NONE;
    visuals
}

/// 从系统字体目录加载中文字体(Windows 常见位置,按序尝试)。
fn load_cjk_font() -> Option<Vec<u8>> {
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhl.ttc",
        "C:\\Windows\\Fonts\\Deng.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            tracing::info!(path, "已加载中文字体");
            return Some(bytes);
        }
    }
    tracing::warn!("未找到系统中文字体,中文可能显示为方块");
    None
}
