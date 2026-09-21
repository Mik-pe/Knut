//! The workbench's visual language: one place that decides colour, weight
//! and glyphs for every surface.
//!
//! Two rules shape this module.
//!
//! **Degrade, never lie.** Colour is a rendering detail, so a terminal
//! without truecolor gets the nearest 256- or 16-colour palette, and a
//! terminal with no colour at all (or `NO_COLOR`) gets weight and glyph
//! contrast instead. Status is always also carried by text, so nothing
//! becomes unreadable when the palette collapses.
//!
//! **Pure and testable.** The theme is plain data plus lookup functions;
//! it performs no I/O, so rendering stays deterministic under
//! `TestBackend` and every style decision can be asserted directly.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// A 24-bit colour, before the terminal's capability is considered.
pub type Rgb = (u8, u8, u8);

/// How much colour the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    /// 24-bit `COLORTERM=truecolor`.
    TrueColor,
    /// xterm-256.
    Ansi256,
    /// The classic sixteen.
    Ansi16,
    /// No colour: weight and glyphs carry the structure.
    Mono,
}

impl ColorLevel {
    /// Detect from the environment, honouring explicit overrides.
    ///
    /// `NO_COLOR` is respected as the widely-adopted opt-out;
    /// `KNUT_TUI_COLORS` overrides detection for terminals that
    /// misreport themselves or for scripted screenshots.
    pub fn detect() -> Self {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// The detection rule, separated from the environment so it can be
    /// tested without mutating process state.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        if let Some(explicit) = get("KNUT_TUI_COLORS") {
            return match explicit.to_ascii_lowercase().as_str() {
                "truecolor" | "24bit" => ColorLevel::TrueColor,
                "256" | "ansi256" => ColorLevel::Ansi256,
                "16" | "ansi16" => ColorLevel::Ansi16,
                "none" | "mono" | "off" => ColorLevel::Mono,
                _ => ColorLevel::detect_terminal(&get),
            };
        }
        if get("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return ColorLevel::Mono;
        }
        ColorLevel::detect_terminal(&get)
    }

    fn detect_terminal(get: &impl Fn(&str) -> Option<String>) -> Self {
        match get("COLORTERM").as_deref() {
            Some("truecolor") | Some("24bit") => return ColorLevel::TrueColor,
            _ => {}
        }
        match get("TERM").as_deref() {
            Some(term) if term.contains("truecolor") || term.contains("direct") => {
                ColorLevel::TrueColor
            }
            Some(term) if term.contains("256color") => ColorLevel::Ansi256,
            Some(term) if term == "dumb" => ColorLevel::Mono,
            Some(_) => ColorLevel::Ansi16,
            None => ColorLevel::Ansi16,
        }
    }

    /// Whether this level paints colour at all.
    pub fn is_color(self) -> bool {
        !matches!(self, ColorLevel::Mono)
    }
}

/// The palette: names for the roles the interface needs, not for hues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Rgb,
    pub bg_panel: Rgb,
    pub bg_raise: Rgb,
    pub bg_sel: Rgb,
    pub border: Rgb,
    pub border_focus: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub faint: Rgb,
    pub cyan: Rgb,
    pub sky: Rgb,
    pub violet: Rgb,
    pub magenta: Rgb,
    pub lime: Rgb,
    pub green: Rgb,
    pub amber: Rgb,
    pub rose: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        // A night-city palette: near-black blues for surfaces, one hot
        // cyan for "the machine is doing something", and a warm rose for
        // anything the user must not miss.
        Self {
            bg: (0x07, 0x0A, 0x10),
            bg_panel: (0x0B, 0x11, 0x19),
            bg_raise: (0x11, 0x1A, 0x25),
            bg_sel: (0x16, 0x23, 0x33),
            border: (0x1E, 0x2C, 0x3C),
            border_focus: (0x22, 0xD3, 0xEE),
            text: (0xDC, 0xE7, 0xF5),
            dim: (0x7A, 0x8C, 0xA3),
            faint: (0x46, 0x56, 0x69),
            cyan: (0x22, 0xD3, 0xEE),
            sky: (0x38, 0xBD, 0xF8),
            violet: (0xA7, 0x8B, 0xFA),
            magenta: (0xF4, 0x72, 0xB6),
            lime: (0xA3, 0xE6, 0x35),
            green: (0x4A, 0xDE, 0x80),
            amber: (0xFB, 0xBF, 0x24),
            rose: (0xFB, 0x71, 0x85),
        }
    }
}

/// Which glyph family to draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Whether the terminal is expected to have box-drawing and the
    /// geometric shapes the design uses.
    pub unicode: bool,
}

impl Glyphs {
    /// The brand mark: a woven knot. Falls back to a plain wordmark on
    /// terminals without the glyphs.
    pub fn knot(&self) -> &'static str {
        if self.unicode { "⟠" } else { "#" }
    }

    /// A filled block, used for the streaming cursor and bars.
    pub fn bar(&self) -> &'static str {
        if self.unicode { "▌" } else { "|" }
    }

    pub fn bullet(&self) -> &'static str {
        if self.unicode { "●" } else { "*" }
    }

    pub fn diamond(&self) -> &'static str {
        if self.unicode { "◆" } else { "<>" }
    }

    /// The composer's prompt sigil.
    pub fn prompt(&self) -> &'static str {
        if self.unicode { "❯" } else { ">" }
    }

    pub fn separator(&self) -> &'static str {
        if self.unicode { "▏" } else { "|" }
    }

    /// A vertical rail for quoted/streaming content.
    pub fn rail(&self) -> &'static str {
        if self.unicode { "▏" } else { "|" }
    }

    /// A running marker.
    pub fn running(&self) -> &'static str {
        if self.unicode { "▸" } else { ">" }
    }

    /// A success marker. Used for both a passed check and a completed task,
    /// because both mean "verified".
    pub fn tick(&self) -> &'static str {
        if self.unicode { "✔" } else { "+" }
    }

    /// A failure marker.
    pub fn cross(&self) -> &'static str {
        if self.unicode { "✘" } else { "x" }
    }

    /// A cancellation marker: distinct from failure.
    pub fn cancelled(&self) -> &'static str {
        if self.unicode { "✕" } else { "x" }
    }

    /// A routing marker.
    pub fn route(&self) -> &'static str {
        if self.unicode { "»" } else { ">" }
    }

    /// A queued/pending marker.
    pub fn pending(&self) -> &'static str {
        if self.unicode { "…" } else { "." }
    }

    /// A question marker: something needs the user.
    pub fn question(&self) -> &'static str {
        "?"
    }

    /// A paused marker.
    pub fn paused(&self) -> &'static str {
        if self.unicode { "=" } else { "=" }
    }
}

/// The resolved theme: palette plus the terminal's capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub level: ColorLevel,
    pub palette: Palette,
    pub glyphs: Glyphs,
    /// Whether panels paint their own background.
    pub paint_background: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self::detect()
    }
}

impl Theme {
    /// Detect the theme for this process.
    pub fn detect() -> Self {
        Self::for_level(ColorLevel::detect())
    }

    /// The plain theme: no colour, background left to the terminal. Used
    /// by tests and by `KNUT_TUI_COLORS=none`.
    pub fn plain() -> Self {
        Self {
            level: ColorLevel::Mono,
            palette: Palette::default(),
            glyphs: Glyphs { unicode: true },
            paint_background: false,
        }
    }

    /// A fixed theme for one capability level.
    pub fn for_level(level: ColorLevel) -> Self {
        Self {
            level,
            palette: Palette::default(),
            glyphs: Glyphs { unicode: true },
            paint_background: level.is_color(),
        }
    }

    /// The plain theme with ASCII glyphs (no Unicode assumptions).
    pub fn plain_ascii() -> Self {
        Self {
            glyphs: Glyphs { unicode: false },
            ..Self::plain()
        }
    }

    /// Resolve a colour for this terminal.
    pub fn color(&self, rgb: Rgb) -> Color {
        match self.level {
            ColorLevel::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            ColorLevel::Ansi256 => Color::Indexed(to_ansi256(rgb)),
            ColorLevel::Ansi16 => to_ansi16(rgb),
            ColorLevel::Mono => Color::Reset,
        }
    }

    /// A foreground style.
    pub fn fg(&self, rgb: Rgb) -> Style {
        if self.level.is_color() {
            Style::default().fg(self.color(rgb))
        } else {
            Style::default()
        }
    }

    /// A background style.
    pub fn bg(&self, rgb: Rgb) -> Style {
        if self.paint_background {
            Style::default().bg(self.color(rgb))
        } else {
            Style::default()
        }
    }

    /// Text colour for body copy.
    pub fn text(&self) -> Style {
        self.fg(self.palette.text)
    }

    /// Dimmed copy.
    pub fn dim(&self) -> Style {
        if self.level.is_color() {
            self.fg(self.palette.dim)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// The faintest useful copy (timestamps, counts).
    pub fn faint(&self) -> Style {
        if self.level.is_color() {
            self.fg(self.palette.faint)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn accent(&self) -> Style {
        self.fg(self.palette.cyan)
    }

    pub fn success(&self) -> Style {
        self.fg(self.palette.green)
    }

    pub fn warn(&self) -> Style {
        self.fg(self.palette.amber)
    }

    pub fn danger(&self) -> Style {
        if self.level.is_color() {
            self.fg(self.palette.rose)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// The panel border style.
    pub fn border(&self) -> Style {
        self.fg(self.palette.border)
    }

    /// The border style for a focused surface.
    pub fn border_focused(&self) -> Style {
        if self.level.is_color() {
            self.fg(self.palette.border_focus)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A spinner frame for the given tick. Braille where available.
    pub fn spinner(&self, tick: u64) -> &'static str {
        const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        if self.glyphs.unicode {
            BRAILLE[(tick as usize) % BRAILLE.len()]
        } else {
            ASCII[(tick as usize) % ASCII.len()]
        }
    }

    /// A left-to-right colour ramp across a string, one span per
    /// character. Gradients are why a wordmark reads as a logo rather
    /// than as text.
    pub fn gradient(&self, text: &str, from: Rgb, to: Rgb) -> Vec<Span<'static>> {
        let chars: Vec<char> = text.chars().collect();
        let last = chars.len().saturating_sub(1).max(1);
        chars
            .into_iter()
            .enumerate()
            .map(|(index, character)| {
                let t = index as f32 / last as f32;
                let rgb = lerp(from, to, t);
                let style = if self.level.is_color() {
                    Style::default().fg(self.color(rgb))
                } else {
                    Style::default()
                };
                Span::styled(character.to_string(), style)
            })
            .collect()
    }

    /// The theme's own 2-stop brand ramp (cyan -> magenta).
    pub fn brand_gradient(&self, text: &str) -> Vec<Span<'static>> {
        self.gradient(text, self.palette.cyan, self.palette.magenta)
    }
}

/// Linear interpolation between two colours.
pub fn lerp(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| {
        let a = a as f32;
        let b = b as f32;
        (a + (b - a) * t).round().clamp(0.0, 255.0) as u8
    };
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// The xterm-256 cube plus greyscale ramp.
pub fn to_ansi256((r, g, b): Rgb) -> u8 {
    // Greys are better served by the 24-step ramp than by the cube.
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r as u16 - 8) * 24 / 247) as u8;
    }
    let axis = |value: u8| -> u8 {
        // 0..=5 levels, with the same thresholds xterm itself uses.
        match value {
            0..=47 => 0,
            48..=114 => 1,
            _ => ((value as u16 - 35) / 40).min(5) as u8,
        }
    };
    16 + 36 * axis(r) + 6 * axis(g) + axis(b)
}

/// The classic sixteen, chosen by nearest colour.
///
/// Distance is weighted toward green, matching how much more the eye
/// resolves in the middle of the spectrum; without the weights, olive
/// and orange collapse into each other on 16-colour terminals.
pub fn to_ansi16(rgb: Rgb) -> Color {
    const TABLE: [(Color, Rgb); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::Red, (170, 0, 0)),
        (Color::Green, (0, 170, 0)),
        (Color::Yellow, (170, 85, 0)),
        (Color::Blue, (0, 0, 170)),
        (Color::Magenta, (170, 0, 170)),
        (Color::Cyan, (0, 170, 170)),
        (Color::Gray, (170, 170, 170)),
        (Color::DarkGray, (85, 85, 85)),
        (Color::LightRed, (255, 85, 85)),
        (Color::LightGreen, (85, 255, 85)),
        (Color::LightYellow, (255, 255, 85)),
        (Color::LightBlue, (85, 85, 255)),
        (Color::LightMagenta, (255, 85, 255)),
        (Color::LightCyan, (85, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];

    let distance = |(r, g, b): Rgb, (r2, g2, b2): Rgb| {
        let dr = r as i32 - r2 as i32;
        let dg = g as i32 - g2 as i32;
        let db = b as i32 - b2 as i32;
        2 * dr * dr + 4 * dg * dg + db * db
    };

    TABLE
        .iter()
        .min_by_key(|(_, candidate)| distance(rgb, *candidate))
        .map(|(color, _)| *color)
        .unwrap_or(Color::Reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn detection_prefers_explicit_override_then_colorterm_then_term() {
        assert_eq!(
            ColorLevel::from_env(env(&[("KNUT_TUI_COLORS", "16")])),
            ColorLevel::Ansi16
        );
        assert_eq!(
            ColorLevel::from_env(env(&[
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-256color")
            ])),
            ColorLevel::TrueColor
        );
        assert_eq!(
            ColorLevel::from_env(env(&[("TERM", "xterm-256color")])),
            ColorLevel::Ansi256
        );
        assert_eq!(
            ColorLevel::from_env(env(&[("TERM", "xterm")])),
            ColorLevel::Ansi16
        );
        // An unknown override falls back to detection rather than
        // silently choosing a level the terminal may not support.
        assert_eq!(
            ColorLevel::from_env(env(&[
                ("KNUT_TUI_COLORS", "plaid"),
                ("TERM", "screen-256color")
            ])),
            ColorLevel::Ansi256
        );
    }

    #[test]
    fn no_color_disables_colour_but_not_structure() {
        let level = ColorLevel::from_env(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]));
        assert_eq!(level, ColorLevel::Mono);

        let theme = Theme::for_level(level);
        // Still a full palette: structure comes from weight and glyphs.
        assert!(!theme.level.is_color());
        assert_eq!(theme.color(theme.palette.cyan), Color::Reset);
        assert!(theme.glyphs.unicode);
    }

    #[test]
    fn an_empty_no_color_is_not_an_opt_out() {
        // The convention is that only a non-empty value opts out.
        let level = ColorLevel::from_env(env(&[("NO_COLOR", ""), ("TERM", "xterm-256color")]));
        assert_eq!(level, ColorLevel::Ansi256);
    }

    #[test]
    fn a_dumb_terminal_gets_no_colour() {
        assert_eq!(
            ColorLevel::from_env(env(&[("TERM", "dumb")])),
            ColorLevel::Mono
        );
    }

    #[test]
    fn truecolor_is_passed_through_bit_for_bit() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        assert_eq!(
            theme.color((0x22, 0xD3, 0xEE)),
            Color::Rgb(0x22, 0xD3, 0xEE)
        );
    }

    #[test]
    fn ansi256_mapping_is_sane_for_the_brand_colours() {
        // Pure greys use the ramp; near primaries land in the cube.
        assert_eq!(to_ansi256((0, 0, 0)), 16);
        assert_eq!(to_ansi256((255, 255, 255)), 231);
        assert_eq!(to_ansi256((0xFF, 0, 0)), 196);
        assert_eq!(to_ansi256((0, 0xFF, 0)), 46);
        // Cyan-ish brand colour must stay recognisably cyan in the cube.
        let index = to_ansi256((0x22, 0xD3, 0xEE));
        assert!((16..=231).contains(&index));
        assert_eq!(index, 45, "brand cyan should map to the cyan cube cell");
        // Two different greys must not collapse onto the same entry.
        assert_ne!(to_ansi256((40, 40, 40)), to_ansi256((80, 80, 80)));
    }

    #[test]
    fn ansi16_keeps_roles_distinguishable() {
        let palette = Palette::default();
        // The roles that must never be confused on a 16-colour terminal.
        assert!(matches!(
            to_ansi16(palette.green),
            Color::Green | Color::LightGreen
        ));
        assert!(matches!(
            to_ansi16(palette.rose),
            Color::Red | Color::LightRed
        ));
        assert!(matches!(
            to_ansi16(palette.amber),
            Color::Yellow | Color::LightYellow
        ));
        assert!(matches!(
            to_ansi16(palette.cyan),
            Color::Cyan | Color::LightCyan
        ));
        // Success and failure must stay apart.
        assert_ne!(to_ansi16(palette.green), to_ansi16(palette.rose));
    }

    #[test]
    fn lerp_hits_both_ends_and_the_middle() {
        assert_eq!(lerp((0, 0, 0), (255, 255, 255), 0.0), (0, 0, 0));
        assert_eq!(lerp((0, 0, 0), (255, 255, 255), 1.0), (255, 255, 255));
        assert_eq!(lerp((0, 0, 0), (255, 255, 255), 0.5), (128, 128, 128));
        // Out-of-range input is clamped rather than wrapping.
        assert_eq!(lerp((0, 0, 0), (255, 255, 255), 2.0), (255, 255, 255));
        assert_eq!(lerp((0, 0, 0), (255, 255, 255), -1.0), (0, 0, 0));
    }

    #[test]
    fn a_gradient_covers_every_character_once() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let spans = theme.gradient("KNUT", theme.palette.cyan, theme.palette.magenta);
        assert_eq!(spans.len(), 4);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "KNUT");
        // Ends are exactly the ramp endpoints.
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(0x22, 0xD3, 0xEE)));
        assert_eq!(spans[3].style.fg, Some(Color::Rgb(0xF4, 0x72, 0xB6)));
        // And it is monotonic in between: no duplicated stops.
        assert_ne!(spans[1].style.fg, spans[2].style.fg);
    }

    #[test]
    fn a_monochrome_gradient_still_emits_the_text() {
        let theme = Theme::plain();
        let spans = theme.gradient("knut", theme.palette.cyan, theme.palette.magenta);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "knut");
        assert!(spans.iter().all(|span| span.style.fg.is_none()));
    }

    #[test]
    fn spinners_are_periodic_and_ascii_safe() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let frames: Vec<&str> = (0..10).map(|tick| theme.spinner(tick)).collect();
        let mut unique = frames.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 10, "braille spinner frames must differ");
        assert_eq!(theme.spinner(10), theme.spinner(0), "frames cycle");

        let ascii = Theme::plain_ascii();
        assert!(
            ascii
                .spinner(0)
                .chars()
                .all(|character| character.is_ascii()),
            "ascii fallback must not emit braille"
        );
        assert!(ascii.glyphs.knot().is_ascii());
        assert!(ascii.glyphs.prompt().is_ascii());
    }

    #[test]
    fn painting_background_follows_colour_capability() {
        let rich = Theme::for_level(ColorLevel::Ansi256);
        assert!(rich.paint_background);
        assert_eq!(
            rich.bg(rich.palette.bg_panel).bg,
            Some(Color::Indexed(to_ansi256(rich.palette.bg_panel)))
        );

        let plain = Theme::plain();
        assert!(!plain.paint_background);
        assert_eq!(plain.bg(plain.palette.bg_panel).bg, None);
    }

    #[test]
    fn dim_and_danger_carry_meaning_without_colour() {
        let plain = Theme::plain();
        assert!(plain.dim().add_modifier.contains(Modifier::DIM));
        assert!(plain.danger().add_modifier.contains(Modifier::BOLD));
        // And text remains text: every role still renders content.
        assert!(plain.text().fg.is_none());
    }
}
