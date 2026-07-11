use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

static COMPOSER_BG: OnceLock<Option<Color>> = OnceLock::new();
static PICKER_BG: OnceLock<Option<Color>> = OnceLock::new();
static TOOL_HOVER_BG: OnceLock<Option<Color>> = OnceLock::new();
static TERMINAL_IS_LIGHT: OnceLock<bool> = OnceLock::new();
static TERMINAL_BG: OnceLock<Option<(u8, u8, u8)>> = OnceLock::new();
const CODE_CONTRAST_RATIO: f32 = 7.0;

pub fn init_terminal_bg(bg: Option<(u8, u8, u8)>) {
    let _ = TERMINAL_BG.set(bg);
    let _ = TERMINAL_IS_LIGHT.set(bg.is_some_and(is_light));
    let _ = COMPOSER_BG.set(surface(bg, 0.04, 0.12));
    let _ = PICKER_BG.set(picker_surface(bg));
    let _ = TOOL_HOVER_BG.set(surface(bg, 0.025, 0.07));
}

fn picker_surface(bg: Option<(u8, u8, u8)>) -> Option<Color> {
    bg.map(|bg| {
        let alpha = if is_light(bg) { 0.06 } else { 0.18 };
        let (r, g, b) = blend((0, 0, 0), bg, alpha);
        Color::Rgb(r, g, b)
    })
}

fn surface(bg: Option<(u8, u8, u8)>, light_alpha: f32, dark_alpha: f32) -> Option<Color> {
    bg.map(|bg| {
        let (top, alpha) = if is_light(bg) {
            ((0, 0, 0), light_alpha)
        } else {
            ((255, 255, 255), dark_alpha)
        };
        let (r, g, b) = blend(top, bg, alpha);
        Color::Rgb(r, g, b)
    })
}

fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 128.0
}

fn blend(top: (u8, u8, u8), bottom: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let channel = |t: u8, b: u8| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let blended = (f32::from(t) * alpha + f32::from(b) * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8;
        blended
    };
    (
        channel(top.0, bottom.0),
        channel(top.1, bottom.1),
        channel(top.2, bottom.2),
    )
}

pub fn dim() -> Style {
    Style::new().fg(adaptive_color((70, 70, 70), (150, 150, 150)))
}

pub fn thinking_pulse(frame: usize) -> Style {
    thinking_pulse_style(
        frame,
        terminal_is_light(),
        TERMINAL_BG.get().is_some_and(Option::is_some),
    )
}

fn thinking_pulse_style(frame: usize, light_background: bool, background_known: bool) -> Style {
    const STEPS: usize = 24;

    let phase = (frame / 2) % (STEPS * 2);
    let strength = if phase <= STEPS {
        phase
    } else {
        STEPS * 2 - phase
    };
    if !background_known {
        let style = Style::new().fg(Color::Reset);
        return if strength < STEPS / 3 {
            style.add_modifier(Modifier::DIM)
        } else if strength > STEPS * 2 / 3 {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
    }
    let channel = if light_background {
        110 - 90 * strength / STEPS
    } else {
        145 + 90 * strength / STEPS
    };
    let channel = u8::try_from(channel).expect("pulse color is in range");
    Style::new().fg(Color::Rgb(channel, channel, channel))
}

pub fn bold() -> Style {
    Style::new().fg(primary_text()).add_modifier(Modifier::BOLD)
}

pub fn success() -> Style {
    Style::new().fg(adaptive_color((0, 105, 45), (80, 210, 110)))
}

pub fn error() -> Style {
    Style::new().fg(adaptive_color((180, 35, 35), (255, 100, 100)))
}

pub fn running() -> Style {
    Style::new().fg(adaptive_color((0, 95, 125), (80, 205, 225)))
}

pub fn tool_running() -> Style {
    Style::new().fg(primary_text())
}

pub fn approval() -> Style {
    Style::new().fg(adaptive_color((135, 85, 0), (245, 200, 70)))
}

pub fn key() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

pub fn picker_selected() -> Style {
    Style::new().fg(primary_text()).add_modifier(Modifier::BOLD)
}

pub fn picker_muted() -> Style {
    Style::new().fg(adaptive_color((65, 65, 65), (170, 170, 170)))
}

pub fn code() -> Style {
    Style::new().fg(adaptive_color((0, 90, 120), (100, 210, 230)))
}

pub fn inline_code() -> Style {
    code().add_modifier(Modifier::BOLD)
}

pub fn link() -> Style {
    Style::new()
        .fg(adaptive_color((30, 75, 180), (110, 165, 255)))
        .add_modifier(Modifier::UNDERLINED)
}

pub fn heading(level: u8) -> Style {
    let color = if terminal_is_light() {
        Color::Rgb(85, 45, 140)
    } else {
        Color::Rgb(198, 160, 246)
    };
    let mut style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    if level <= 2 {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

pub fn quote() -> Style {
    Style::new()
        .fg(adaptive_color((75, 75, 75), (160, 160, 160)))
        .add_modifier(Modifier::ITALIC)
}

pub fn rule() -> Style {
    Style::new().fg(adaptive_color((105, 105, 105), (125, 125, 125)))
}

pub fn table_border() -> Style {
    rule()
}

pub fn terminal_is_light() -> bool {
    TERMINAL_IS_LIGHT.get().copied().unwrap_or(false)
}

pub fn code_color(rgb: (u8, u8, u8)) -> Color {
    let background = TERMINAL_BG.get().copied().flatten().unwrap_or_else(|| {
        if terminal_is_light() {
            (245, 245, 245)
        } else {
            (32, 32, 32)
        }
    });
    let adjusted = contrasting_code_rgb(rgb, background);
    Color::Rgb(adjusted.0, adjusted.1, adjusted.2)
}

fn contrasting_code_rgb(rgb: (u8, u8, u8), background: (u8, u8, u8)) -> (u8, u8, u8) {
    if contrast_ratio(rgb, background) >= CODE_CONTRAST_RATIO {
        return rgb;
    }

    let target = if is_light(background) {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    };
    for step in 1..=20_u8 {
        let adjusted = blend(target, rgb, f32::from(step) / 20.0);
        if contrast_ratio(adjusted, background) >= CODE_CONTRAST_RATIO {
            return adjusted;
        }
    }
    target
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (lighter, darker) = {
        let a = relative_luminance(a);
        let b = relative_luminance(b);
        if a > b { (a, b) } else { (b, a) }
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    let linear = |channel: u8| {
        let value = f32::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

pub fn assistant_bullet() -> Style {
    Style::new().fg(primary_text())
}

pub fn placeholder() -> Style {
    Style::new().fg(adaptive_color((100, 100, 100), (130, 130, 130)))
}

pub fn composer_bg() -> Style {
    match COMPOSER_BG.get().copied().flatten() {
        Some(color) => Style::new().bg(color),
        None => Style::new(),
    }
}

pub fn picker_bg() -> Style {
    match PICKER_BG.get().copied().flatten() {
        Some(color) => Style::new().bg(color),
        None => Style::new(),
    }
}

pub fn scroll_button() -> Style {
    picker_bg().fg(primary_text())
}

pub fn tool_hover() -> Style {
    match TOOL_HOVER_BG.get().copied().flatten() {
        Some(color) => Style::new().bg(color),
        None => Style::new().bg(Color::Rgb(35, 35, 35)),
    }
}

pub fn prompt() -> Style {
    bold()
}

fn primary_text() -> Color {
    adaptive_color((25, 25, 25), (235, 235, 235))
}

fn adaptive_color(light: (u8, u8, u8), dark: (u8, u8, u8)) -> Color {
    let color = if terminal_is_light() { light } else { dark };
    Color::Rgb(color.0, color.1, color.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_colors_reach_readable_contrast_on_dark_and_light_backgrounds() {
        for background in [(5, 5, 8), (250, 250, 250)] {
            let adjusted = contrasting_code_rgb((90, 95, 105), background);
            assert!(contrast_ratio(adjusted, background) >= CODE_CONTRAST_RATIO);
        }
    }

    #[test]
    fn already_readable_code_colors_are_preserved() {
        let color = (220, 180, 120);
        assert_eq!(contrasting_code_rgb(color, (0, 0, 0)), color);
    }

    #[test]
    fn thinking_pulse_has_visible_range_on_light_and_dark_backgrounds() {
        for (is_light, background) in [(true, (250, 250, 250)), (false, (5, 5, 8))] {
            let weak = thinking_pulse_style(0, is_light, true)
                .fg
                .expect("pulse has a foreground");
            let strong = thinking_pulse_style(48, is_light, true)
                .fg
                .expect("pulse has a foreground");
            let (Color::Rgb(weak, _, _), Color::Rgb(strong, _, _)) = (weak, strong) else {
                panic!("known backgrounds use RGB pulse colors");
            };
            assert!(weak.abs_diff(strong) >= 80);
            assert!(contrast_ratio((weak, weak, weak), background) >= 4.5);
            assert!(contrast_ratio((strong, strong, strong), background) >= 4.5);
        }
    }

    #[test]
    fn thinking_pulse_fallback_uses_terminal_foreground_and_modifier_phases() {
        let weak = thinking_pulse_style(0, false, false);
        let middle = thinking_pulse_style(24, false, false);
        let strong = thinking_pulse_style(48, false, false);

        assert_eq!(weak.fg, Some(Color::Reset));
        assert!(weak.add_modifier.contains(Modifier::DIM));
        assert!(!middle.add_modifier.contains(Modifier::DIM | Modifier::BOLD));
        assert!(strong.add_modifier.contains(Modifier::BOLD));
    }
}
