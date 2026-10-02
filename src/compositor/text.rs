//! Minimal text rasterisation for compositor chrome — currently the overview's
//! window labels.
//!
//! One font, laid out left to right with no shaping or kerning, drawn into an
//! [`Rgba`] canvas that the caller wraps in a `MemoryRenderBuffer` — the same
//! buffer type the cursor sprites already use, so nothing new is needed on the
//! render side. Titles are short and drawn small; full text shaping would buy
//! nothing visible here.
//!
//! No font is bundled. The first readable file in [`FONT_CANDIDATES`] wins, and
//! if none of them exist the compositor simply draws no labels rather than
//! failing to start.

use std::sync::OnceLock;

use fontdue::{Font, FontSettings};

/// Where to look for a UI font, best first. These are the sans-serif faces that
/// ship with the common font packages; a box with none of them gets no labels.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation-fonts/LiberationSans-Regular.ttf",
    "/usr/share/fonts/gnu-free/FreeSans.otf",
    "/usr/share/fonts/TTF/Vera.ttf",
];

/// Loaded once on first use. `None` means no candidate existed — checked every
/// call but only ever logged once, since the answer cannot change at runtime.
static FONT: OnceLock<Option<Font>> = OnceLock::new();

fn font() -> Option<&'static Font> {
    FONT.get_or_init(|| {
        for path in FONT_CANDIDATES {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            match Font::from_bytes(data, FontSettings::default()) {
                Ok(font) => {
                    tracing::debug!("overview labels using font {path}");
                    return Some(font);
                }
                Err(error) => tracing::warn!("ignoring unreadable font {path}: {error}"),
            }
        }
        tracing::warn!(
            "no UI font found in {FONT_CANDIDATES:?}; overview labels will be icon-only"
        );
        None
    })
    .as_ref()
}

/// Width `text` would occupy at `px`, in pixels. Used to centre a label before
/// rasterising it.
fn line_width(font: &Font, text: &str, px: f32) -> f32 {
    text.chars()
        .map(|ch| font.metrics(ch, px).advance_width)
        .sum()
}

/// Shorten `text` with a trailing ellipsis until it fits `max_width` pixels at
/// `px`. Returns the text unchanged when it already fits.
fn ellipsize(font: &Font, text: &str, px: f32, max_width: f32) -> String {
    if line_width(font, text, px) <= max_width {
        return text.to_string();
    }
    let ellipsis_width = line_width(font, "…", px);
    let mut kept = String::new();
    let mut width = 0.0;
    for ch in text.chars() {
        let advance = font.metrics(ch, px).advance_width;
        if width + advance + ellipsis_width > max_width {
            break;
        }
        width += advance;
        kept.push(ch);
    }
    kept.push('…');
    kept
}

/// A premultiplied-RGBA pixel canvas, the common currency between the font
/// rasteriser, the icon loader and whatever composites them.
#[derive(Clone)]
pub struct Rgba {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Rgba {
    /// A canvas filled with a single premultiplied colour.
    pub fn filled(width: usize, height: usize, rgba: [f32; 4]) -> Self {
        let a = rgba[3];
        let premultiplied = [
            (rgba[0] * a * 255.0) as u8,
            (rgba[1] * a * 255.0) as u8,
            (rgba[2] * a * 255.0) as u8,
            (a * 255.0) as u8,
        ];
        Self {
            width,
            height,
            pixels: premultiplied
                .iter()
                .copied()
                .cycle()
                .take(width * height * 4)
                .collect(),
        }
    }

    /// Source-over composite of `src` at (`x`, `y`). Both sides are
    /// premultiplied, so this is the plain `src + dst * (1 - src_a)` form.
    pub fn blend(&mut self, src: &Rgba, x: i32, y: i32) {
        for row in 0..src.height {
            let dy = y + row as i32;
            if dy < 0 || dy as usize >= self.height {
                continue;
            }
            for col in 0..src.width {
                let dx = x + col as i32;
                if dx < 0 || dx as usize >= self.width {
                    continue;
                }
                let s = (row * src.width + col) * 4;
                let d = (dy as usize * self.width + dx as usize) * 4;
                let inv = 255 - src.pixels[s + 3] as u32;
                for channel in 0..4 {
                    let value = src.pixels[s + channel] as u32
                        + (self.pixels[d + channel] as u32 * inv + 127) / 255;
                    self.pixels[d + channel] = value.min(255) as u8;
                }
            }
        }
    }
}

/// Height of one line at `px` — ascent to descent, the box `draw_line` fills.
pub fn line_height(px: f32) -> Option<i32> {
    let metrics = font()?.horizontal_line_metrics(px)?;
    Some((metrics.ascent - metrics.descent).ceil().max(1.0) as i32)
}

/// Draw one line of `text` at `px` into `canvas`, with its top-left at
/// (`x`, `y`), tinted `rgb` and ellipsized to `max_width` pixels.
///
/// Does nothing when there is no font, so a box without one simply gets
/// icon-only labels instead of failing.
pub fn draw_line(
    canvas: &mut Rgba,
    x: i32,
    y: i32,
    text: &str,
    px: f32,
    max_width: i32,
    rgb: [f32; 3],
) {
    let Some(font) = font() else {
        return;
    };
    let text = text.trim();
    if text.is_empty() || max_width <= 0 {
        return;
    }
    let text = ellipsize(font, text, px, max_width as f32);
    if text == "…" {
        // Too narrow for even one character: a lone ellipsis says less than
        // blank space does.
        return;
    }
    let Some(metrics) = font.horizontal_line_metrics(px) else {
        return;
    };

    // The baseline sits at `ascent` below the top of the line box; each glyph
    // is then placed relative to it by its own `ymin`.
    let baseline = y as f32 + metrics.ascent;
    let mut pen = x as f32;
    for ch in text.chars() {
        let (glyph, coverage) = font.rasterize(ch, px);
        let left = (pen + glyph.xmin as f32).round() as i32;
        let top = (baseline - (glyph.height as f32 + glyph.ymin as f32)).round() as i32;
        for row in 0..glyph.height {
            let dy = top + row as i32;
            if dy < 0 || dy as usize >= canvas.height {
                continue;
            }
            for col in 0..glyph.width {
                let dx = left + col as i32;
                if dx < 0 || dx as usize >= canvas.width {
                    continue;
                }
                let alpha = coverage[row * glyph.width + col];
                if alpha == 0 {
                    continue;
                }
                // Premultiplied source-over of a solid tint at `alpha`.
                let a = alpha as u32;
                let offset = (dy as usize * canvas.width + dx as usize) * 4;
                let inv = 255 - a;
                for (channel, tint) in rgb.iter().enumerate() {
                    let src = (tint * alpha as f32) as u32;
                    let value = src + (canvas.pixels[offset + channel] as u32 * inv + 127) / 255;
                    canvas.pixels[offset + channel] = value.min(255) as u8;
                }
                let value = a + (canvas.pixels[offset + 3] as u32 * inv + 127) / 255;
                canvas.pixels[offset + 3] = value.min(255) as u8;
            }
        }
        pen += glyph.advance_width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the layout maths, which is the only part that can silently go
    /// wrong: a wide string must come back ellipsized and within budget, and a
    /// short one must come back untouched. Skips itself on a box with no font
    /// rather than failing — the fallback path is deliberate.
    #[test]
    fn long_text_is_ellipsized_to_fit() {
        let Some(font) = font() else {
            return;
        };
        let px = 14.0;
        let short = ellipsize(font, "fish", px, 400.0);
        assert_eq!(short, "fish");

        let long = "a very long window title that will certainly not fit in the box";
        let cut = ellipsize(font, long, px, 100.0);
        assert!(cut.ends_with('…'), "{cut:?}");
        assert!(cut.chars().count() < long.chars().count());
        assert!(line_width(font, &cut, px) <= 100.0);
    }
}
