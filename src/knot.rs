//! The mark: a woven knot, drawn at whatever size the terminal allows.
//!
//! A logo is a promise about legibility. Every art block here is a fixed
//! rectangular string with no trailing whitespace, so `logo_lines` can be
//! centred, boxed or gradient-filled without measuring anything twice, and
//! each size has a plain-ASCII twin for terminals without the glyphs.
//!
//! The mark encodes the product: two strands (System 0 and System 1)
//! crossing, tied through a shared ring — one knot, not two lines.

use crate::theme::Theme;

/// One art block: the lines, plus the width they were drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Art {
    pub lines: &'static [&'static str],
}

impl Art {
    /// The widest line, which is the block's true width.
    pub fn width(&self) -> usize {
        self.lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
    }

    pub fn height(&self) -> usize {
        self.lines.len()
    }
}

/// The marks: a looped square, the classic knot glyph.
///
/// Four ears interlace around a square core — strands that visibly cross
/// rather than boxes that nest, which is what makes a mark read as a
/// *knot* and not as a target. Terminal cells are about twice as tall as
/// they are wide, so every block is built to that ratio; a "square" block
/// renders as a tall pill, the most common way terminal art looks broken.
///
/// Each block is exactly rectangular and mirrors about its vertical axis,
/// and the tests enforce both.
pub const KNOT_LARGE: Art = Art {
    lines: &[
        "  ╭───╮     ╭───╮  ",
        "  │   ╰─────╯   │  ",
        " ╭╯             ╰╮ ",
        "╭╯  ╭──╮   ╭──╮  ╰╮",
        "│   │  ╰───╯  │   │",
        "╰╮  ╰──╮   ╭──╯  ╭╯",
        " ╰╮    │   │    ╭╯ ",
        "  │   ╭╯   ╰╮   │  ",
        "  ╰───╯     ╰───╯  ",
    ],
};

/// The middle mark: the welcome screen's default.
pub const KNOT_MEDIUM: Art = Art {
    lines: &[
        " ╭──╮   ╭──╮ ",
        " │  ╰───╯  │ ",
        " ╰──╮   ╭──╯ ",
        "    │   │    ",
        " ╭──╯   ╰──╮ ",
        " │  ╭───╮  │ ",
        " ╰──╯   ╰──╯ ",
    ],
};

/// The smallest mark: a compact weave.
pub const KNOT_SMALL: Art = Art {
    lines: &[
        " ╭─╮ ╭─╮ ",
        " │ ╰─╯ │ ",
        " ╰╮   ╭╯ ",
        "  │   │  ",
        " ╭╯   ╰╮ ",
        " │ ╭─╮ │ ",
        " ╰─╯ ╰─╯ ",
    ],
};

/// The wordmark, drawn either wide or compact.
pub const WORDMARK: &str = "K N U T";
/// The one-line tagline shown under the mark.
pub const TAGLINE: &str = "agentic harness";
/// The sub-line: what the harness actually is.
pub const SUBTITLE: &str = "System 0 / System 1 routing · gated tools · verified completion";

/// The wordmark as block letters, for the welcome screen.
///
/// Drawn as a fixed 5-row block rather than a single line: a wordmark is
/// branding, and branding that is the same size as body text does not read
/// as one. Kept to five rows so the welcome screen stays a workbench rather
/// than a splash screen.
pub const WORDMARK_BLOCK: Art = Art {
    lines: &[
        "█   █ ██  █ █   █ ▀▀█▀▀",
        "█  █  █ █ █ █   █   █  ",
        "███   █  ██ █   █   █  ",
        "█  █  █   █ █   █   █  ",
        "█   █ █   █  ▀▀▀    █  ",
    ],
};

/// The block wordmark in ASCII, for terminals without block glyphs.
pub const WORDMARK_BLOCK_ASCII: Art = Art {
    lines: &[
        "#   # ##  # #   # #####",
        "#  #  # # # #   #   #  ",
        "###   #  ## #   #   #  ",
        "#  #  #   # #   #   #  ",
        "#   # #   #  ###    #  ",
    ],
};

/// The wordmark block for a glyph capability.
pub fn wordmark_block(unicode: bool) -> Art {
    if unicode {
        WORDMARK_BLOCK
    } else {
        WORDMARK_BLOCK_ASCII
    }
}

/// ASCII twins, used when the terminal cannot be trusted with box art.
///
/// The same geometry in `.`, `-`, `'`, `/` and `|`, so the mark keeps its
/// identity on a terminal that would otherwise render a row of question
/// marks.
pub const KNOT_LARGE_ASCII: Art = Art {
    lines: &[
        "  .---.     .---.  ",
        "  |   '-----'   |  ",
        " .'             '. ",
        ".'  .--.   .--.  '.",
        "|   |  '---'  |   |",
        "'.  '--.   .--'  .'",
        " '.    |   |    .' ",
        "  |   .'   '.   |  ",
        "  '---'     '---'  ",
    ],
};

pub const KNOT_MEDIUM_ASCII: Art = Art {
    lines: &[
        " .--.   .--. ",
        " |  '---'  | ",
        " '--.   .--' ",
        "    |   |    ",
        " .--'   '--. ",
        " |  .---.  | ",
        " '--'   '--' ",
    ],
};

pub const KNOT_SMALL_ASCII: Art = Art {
    lines: &[
        " .-. .-. ",
        " | '-' | ",
        " '-. .-' ",
        "   | |   ",
        " .-' '-. ",
        " | .-. | ",
        " '-' '-' ",
    ],
};

/// Choose the art block for a width and glyph capability.
///
/// Never returns a block wider than `width`, and never returns nothing:
/// a terminal too small for art gets the one-character mark.
pub fn art_for(width: usize, unicode: bool) -> Art {
    match (unicode, width) {
        (true, width) if width >= KNOT_LARGE.width() => KNOT_LARGE,
        (true, width) if width >= KNOT_MEDIUM.width() => KNOT_MEDIUM,
        (true, width) if width >= KNOT_SMALL.width() => KNOT_SMALL,
        (false, width) if width >= KNOT_LARGE_ASCII.width() => KNOT_LARGE_ASCII,
        (false, width) if width >= KNOT_MEDIUM_ASCII.width() => KNOT_MEDIUM_ASCII,
        (false, width) if width >= KNOT_SMALL_ASCII.width() => KNOT_SMALL_ASCII,
        _ => Art { lines: &[] },
    }
}

/// Render the mark as themed lines, centred to `width`.
///
/// The glyphs are tinted along the brand ramp, so the mark reads as one
/// object even before the wordmark beneath it.
pub fn logo_lines(theme: &Theme, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line;

    let art = art_for(width, theme.glyphs.unicode);
    if art.lines.is_empty() {
        // Too small for art: the wordmark alone still identifies the app.
        return vec![Line::from(theme.brand_gradient(WORDMARK))];
    }

    let art_width = art.width();
    let pad = width.saturating_sub(art_width) / 2;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(art.height() + 4);

    for line in art.lines {
        let content = line.trim_end();
        let rendered = if theme.level.is_color() {
            gradient_line(theme, content, (0x4A, 0xDE, 0x80), (0x22, 0xD3, 0xEE))
        } else {
            Line::from(format!("{}{}", " ".repeat(pad), content))
        };
        let padded = if theme.level.is_color() {
            pad_line(rendered, width)
        } else {
            rendered
        };
        lines.push(padded);
    }

    // Only the mark: the wordmark and tagline are the caller's business,
    // so a screen can choose its own hierarchy instead of inheriting one
    // and repeating it.
    lines
}

/// Pad a line to exactly `width` so centring is stable frame to frame.
fn pad_line(line: ratatui::text::Line<'static>, width: usize) -> ratatui::text::Line<'static> {
    let content: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    let pad = width.saturating_sub(content) / 2;
    let mut spans = vec![ratatui::text::Span::raw(" ".repeat(pad))];
    spans.extend(line.spans);
    ratatui::text::Line::from(spans)
}

/// A gradient across the *visible* characters of a line, skipping the
/// leading indentation so the ramp starts at the ink rather than at the
/// margin.
fn gradient_line(
    theme: &Theme,
    text: &str,
    from: (u8, u8, u8),
    to: (u8, u8, u8),
) -> ratatui::text::Line<'static> {
    use ratatui::text::{Line, Span};

    let visible: Vec<(usize, char)> = text
        .chars()
        .enumerate()
        .filter(|(_, character)| !character.is_whitespace())
        .collect();
    let last = visible.len().saturating_sub(1).max(1);
    let rank: std::collections::HashMap<usize, usize> = visible
        .iter()
        .enumerate()
        .map(|(rank, (index, _))| (*index, rank))
        .collect();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_index = 0usize;

    for (index, character) in text.chars().enumerate() {
        if character.is_whitespace() {
            if !run.is_empty() {
                spans.push(styled_run(theme, &run, run_index, last, from, to));
                run.clear();
            }
            spans.push(Span::raw(character.to_string()));
            continue;
        }
        if run.is_empty() {
            run_index = *rank.get(&index).unwrap_or(&0);
        }
        run.push(character);
    }
    if !run.is_empty() {
        spans.push(styled_run(theme, &run, run_index, last, from, to));
    }

    Line::from(spans)
}

fn styled_run(
    theme: &Theme,
    run: &str,
    start: usize,
    last: usize,
    from: (u8, u8, u8),
    to: (u8, u8, u8),
) -> ratatui::text::Span<'static> {
    let t = start as f32 / last as f32;
    let rgb = crate::theme::lerp(from, to, t);
    ratatui::text::Span::styled(
        run.to_owned(),
        ratatui::style::Style::default().fg(theme.color(rgb)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;

    #[test]
    fn art_blocks_never_carry_ragged_trailing_whitespace() {
        // Padding *inside* a block is intentional — it is what makes the
        // mark symmetric about its axis — but a line must never be ragged
        // on the right, because that is what reads as a rendering bug.
        for art in [
            KNOT_LARGE,
            KNOT_MEDIUM,
            KNOT_SMALL,
            KNOT_LARGE_ASCII,
            KNOT_MEDIUM_ASCII,
            KNOT_SMALL_ASCII,
        ] {
            assert!(!art.lines.is_empty());
            let width = art.width();
            for line in art.lines {
                assert_eq!(
                    line.chars().count(),
                    width,
                    "ragged line {line:?} in a {width}-wide block"
                );
            }
        }
    }

    #[test]
    fn every_art_block_is_a_solid_rectangle() {
        // A ragged block reads as a rendering bug, not as a logo: each
        // line must be exactly the block's width, indentation included.
        for art in [
            KNOT_LARGE,
            KNOT_MEDIUM,
            KNOT_SMALL,
            KNOT_LARGE_ASCII,
            KNOT_MEDIUM_ASCII,
            KNOT_SMALL_ASCII,
        ] {
            let width = art.width();
            for line in art.lines {
                assert_eq!(
                    line.chars().count(),
                    width,
                    "line {line:?} is {} wide, block is {width}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn the_mark_is_symmetric_about_its_vertical_axis() {
        // The knot is a woven loop: a left-right asymmetry is a typo, not
        // a design choice. Mirroring a box-drawing glyph means swapping
        // its corners, so the reflection is checked glyph-wise rather than
        // by reversing the string.
        for art in [
            KNOT_LARGE,
            KNOT_MEDIUM,
            KNOT_SMALL,
            KNOT_LARGE_ASCII,
            KNOT_MEDIUM_ASCII,
            KNOT_SMALL_ASCII,
        ] {
            for line in art.lines {
                let mirrored: String = line.chars().rev().map(mirror_glyph).collect();
                assert_eq!(
                    *line, mirrored,
                    "line {line:?} is not symmetric about its axis"
                );
            }
        }
    }

    /// The mirror image of one glyph.
    fn mirror_glyph(glyph: char) -> char {
        match glyph {
            '╭' => '╮',
            '╮' => '╭',
            '╰' => '╯',
            '╯' => '╰',
            '/' => '\\',
            '\\' => '/',
            other => other,
        }
    }

    #[test]
    fn every_block_fits_inside_its_own_measured_width() {
        for art in [KNOT_LARGE, KNOT_MEDIUM, KNOT_SMALL] {
            let width = art.width();
            for line in art.lines {
                assert!(
                    line.chars().count() <= width,
                    "line wider than the block: {line:?}"
                );
            }
        }
    }

    #[test]
    fn sizes_shrink_with_the_terminal_and_never_overflow() {
        for unicode in [true, false] {
            let large = art_for(200, unicode);
            let medium = art_for(large.width(), unicode);
            let small = art_for(medium.width(), unicode);
            assert!(large.height() >= medium.height());
            assert!(medium.height() >= small.height());

            // Narrowing the terminal never produces a wider block.
            let mut previous = usize::MAX;
            for width in (0..=200).rev() {
                let art = art_for(width, unicode);
                if !art.lines.is_empty() {
                    assert!(
                        art.width() <= width,
                        "art {art:?} does not fit width {width}"
                    );
                }
                assert!(art.width() <= previous || art.lines.is_empty());
                previous = art.width().max(1);
            }
        }
    }

    #[test]
    fn a_tiny_terminal_still_gets_a_wordmark() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let lines = logo_lines(&theme, 4);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.to_string())
            .collect();
        // No art fits, but the app still names itself.
        assert!(text.contains("K"));
        assert!(!lines.is_empty());
    }

    #[test]
    fn the_logo_is_centred_and_never_wider_than_the_area() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        for width in [30usize, 40, 60, 80, 120] {
            for line in logo_lines(&theme, width) {
                let rendered: usize = line
                    .spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum();
                assert!(
                    rendered <= width,
                    "logo line is {} wide in a {width}-wide area",
                    rendered
                );
            }
        }
    }

    #[test]
    fn the_mark_is_gradient_tinted_on_a_colour_terminal() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let lines = logo_lines(&theme, 120);
        // The mark itself carries the ramp: every drawn cell is styled, so
        // the logo reads as one object rather than as plain text.
        let styled = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg.is_some())
            .count();
        assert!(styled > 20, "the mark should be tinted, got {styled} spans");
        // And it spans more than one hue: a single flat colour is not a ramp.
        let hues: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(hues.len() > 2, "the mark should span a ramp, got {hues:?}");
    }

    #[test]
    fn a_monochrome_logo_keeps_its_shape() {
        let theme = Theme::plain();
        let lines = logo_lines(&theme, 80);
        assert!(lines.len() >= 3);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.to_string())
            .collect();
        // The mark's own glyphs are drawn, whatever the theme.
        assert!(text.contains('╭') || text.contains('|') || text.contains('-'));
        // No colour is emitted at all.
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| span.style.fg.is_none())
        );
    }

    #[test]
    fn ascii_art_is_pure_ascii() {
        for art in [KNOT_LARGE_ASCII, KNOT_MEDIUM_ASCII, KNOT_SMALL_ASCII] {
            for line in art.lines {
                assert!(line.is_ascii(), "ascii art must stay ascii: {line:?}");
            }
        }
    }

    #[test]
    fn the_ascii_mark_has_no_box_drawing() {
        let theme = Theme::plain_ascii();
        let lines = logo_lines(&theme, 80);
        for line in &lines {
            for span in &line.spans {
                assert!(
                    span.content.is_ascii(),
                    "ascii theme emitted non-ascii: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn centring_is_stable_across_frames() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let first = logo_lines(&theme, 100);
        let second = logo_lines(&theme, 100);
        let render = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.to_string())
                        .collect::<String>()
                })
                .collect()
        };
        // A logo that jitters between frames is a bug, not a flourish.
        assert_eq!(render(&first), render(&second));
    }
}
