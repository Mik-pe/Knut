use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::OnceLock;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::theme::{Theme, lerp};

pub(crate) const INTRO_TICKS: u64 = 48;

#[derive(Clone, Copy)]
struct Sample {
    x: f32,
    y: f32,
    depth: f32,
    position: f32,
}

#[derive(Clone, Copy)]
struct Pixel {
    distance: f32,
    depth: f32,
    position: f32,
}

fn strand() -> &'static [Sample] {
    static STRAND: OnceLock<Vec<Sample>> = OnceLock::new();
    STRAND.get_or_init(|| {
        (0..900)
            .map(|index| {
                let position = index as f32 / 900.0;
                let t = position * TAU;
                let radius = 2.0 + (3.0 * t).cos();
                Sample {
                    x: radius * (2.0 * t - FRAC_PI_2).cos(),
                    y: radius * (2.0 * t - FRAC_PI_2).sin(),
                    depth: (3.0 * t).sin(),
                    position,
                }
            })
            .collect()
    })
}

pub fn logo_lines(theme: &Theme, width: usize, height: usize, tick: u64) -> Vec<Line<'static>> {
    let width = width.min(48);
    let height = height.min(16);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let (columns, rows) = (width * 2, height * 4);
    let scale = (columns as f32 / 6.4).min(rows as f32 * 1.25 / 6.4);
    let radius = (scale * 0.16).max(0.6);
    let halo = radius + 0.8;
    let mut pixels: Vec<Option<Pixel>> = vec![None; columns * rows];
    for point in strand() {
        let x = point.x * scale + columns as f32 / 2.0;
        let y = point.y * scale / 1.25 + rows as f32 / 2.0;
        for py in ((y - halo).max(0.0) as usize)..((y + halo + 1.0) as usize).min(rows) {
            for px in ((x - halo).max(0.0) as usize)..((x + halo + 1.0) as usize).min(columns) {
                let distance = (px as f32 + 0.5 - x).powi(2) + (py as f32 + 0.5 - y).powi(2);
                if distance > halo * halo {
                    continue;
                }
                let pixel = &mut pixels[py * columns + px];
                // The front strand masks the crossing, including a narrow gap around it.
                if pixel.is_none_or(|old| {
                    point.depth > old.depth + 0.5
                        || (point.depth >= old.depth - 0.5 && distance < old.distance)
                }) {
                    *pixel = Some(Pixel {
                        distance,
                        depth: point.depth,
                        position: point.position,
                    });
                }
            }
        }
    }
    let phase = if theme.reduced_motion {
        0.18
    } else {
        (tick % 120) as f32 / 120.0
    };
    const DOTS: [[u8; 2]; 4] = [[0, 3], [1, 4], [2, 5], [6, 7]];
    (0..height)
        .map(|row| {
            let spans = (0..width)
                .map(|column| {
                    let mut mask = 0u8;
                    let mut light = 0.0f32;
                    let mut depth = 0.0f32;
                    let mut count = 0.0f32;
                    for (dy, bits) in DOTS.iter().enumerate() {
                        for (dx, bit) in bits.iter().enumerate() {
                            if let Some(pixel) = pixels[(row * 4 + dy) * columns + column * 2 + dx]
                                && pixel.distance <= radius * radius
                            {
                                mask |= 1 << bit;
                                let gap = (pixel.position - phase).abs();
                                let gap = gap.min(1.0 - gap);
                                light = light.max((1.0 - gap / 0.10).max(0.0).powi(2));
                                depth += pixel.depth;
                                count += 1.0;
                            }
                        }
                    }
                    if mask == 0 {
                        return Span::raw(" ");
                    }
                    let base = lerp(
                        theme.palette.sky,
                        theme.palette.cyan,
                        (depth / count + 1.0) / 2.0,
                    );
                    let shaded = lerp(
                        theme.palette.border,
                        base,
                        0.55 + (depth / count + 1.0) * 0.225,
                    );
                    let color = lerp(shaded, theme.palette.text, light * 0.95);
                    let style = if light > 0.5 {
                        theme.fg(color).add_modifier(Modifier::BOLD)
                    } else {
                        theme.fg(color)
                    };
                    let glyph = if theme.glyphs.unicode {
                        char::from_u32(0x2800 + u32::from(mask)).unwrap()
                    } else if count >= 4.0 {
                        '@'
                    } else {
                        '+'
                    };
                    Span::styled(glyph.to_string(), style)
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;

    #[test]
    fn knot_stays_inside_its_box_at_every_supported_size() {
        for theme in [
            Theme::for_level(ColorLevel::TrueColor),
            Theme::plain(),
            Theme::plain_ascii(),
        ] {
            for (width, height) in [(0, 0), (7, 3), (18, 6), (30, 10), (48, 16)] {
                for tick in [0, 27, 75, u64::MAX] {
                    let lines = logo_lines(&theme, width, height, tick);
                    assert_eq!(lines.len(), height);
                    for line in lines {
                        assert!(line.width() <= width);
                        if !theme.glyphs.unicode {
                            assert!(line.spans.iter().all(|span| span.content.is_ascii()));
                        }
                        if !theme.level.is_color() {
                            assert!(line.spans.iter().all(|span| span.style.fg.is_none()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn animation_changes_light_without_moving_the_mark_and_reduced_motion_is_static() {
        let mut theme = Theme::for_level(ColorLevel::TrueColor);
        let first = logo_lines(&theme, 30, 10, 0);
        let later = logo_lines(&theme, 30, 10, 37);
        let text = |lines: &[Line<'_>]| {
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&first), text(&later));
        assert_ne!(first, later);
        assert_eq!(first, logo_lines(&theme, 30, 10, 120));
        theme.reduced_motion = true;
        assert_eq!(
            logo_lines(&theme, 30, 10, 0),
            logo_lines(&theme, 30, 10, 37)
        );
    }
}
