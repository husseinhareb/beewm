use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point, Transform};
use xcursor::{parser::parse_xcursor, CursorTheme};

#[derive(Debug, Clone)]
pub struct CursorSprite {
    pub buffer: MemoryRenderBuffer,
    pub hotspot: Point<i32, Logical>,
}

#[derive(Debug)]
pub struct CursorThemeManager {
    theme: CursorTheme,
    size: u32,
    sprites: HashMap<CursorIcon, CursorSprite>,
}

impl CursorThemeManager {
    pub fn new() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(24);

        Self {
            theme: CursorTheme::load(&theme_name),
            size,
            sprites: HashMap::new(),
        }
    }

    pub fn sprite(&mut self, icon: CursorIcon) -> CursorSprite {
        if let Some(sprite) = self.sprites.get(&icon) {
            return sprite.clone();
        }

        let sprite = self
            .load_sprite(icon)
            .unwrap_or_else(|| fallback_arrow_sprite(self.size));
        self.sprites.insert(icon, sprite.clone());
        sprite
    }

    fn load_sprite(&self, icon: CursorIcon) -> Option<CursorSprite> {
        let icon_path = self
            .theme
            .load_icon(icon.name())
            .or_else(|| icon.alt_names().iter().find_map(|name| self.theme.load_icon(name)))?;

        let contents = std::fs::read(icon_path).ok()?;
        let images = parse_xcursor(&contents)?;
        let image = pick_best_image(&images, self.size)?;

        Some(CursorSprite {
            buffer: MemoryRenderBuffer::from_slice(
                &image.pixels_argb,
                Fourcc::Argb8888,
                (image.width as i32, image.height as i32),
                1,
                Transform::Normal,
                None,
            ),
            hotspot: Point::from((image.xhot as i32, image.yhot as i32)),
        })
    }
}

fn pick_best_image(images: &[xcursor::parser::Image], desired_size: u32) -> Option<&xcursor::parser::Image> {
    images.iter().min_by_key(|image| image.size.abs_diff(desired_size))
}

fn fallback_arrow_sprite(size: u32) -> CursorSprite {
    let size = size.clamp(16, 64);
    let scale = size as f32 / 24.0;
    let points = [
        (1.0 * scale, 1.0 * scale),
        (1.0 * scale, 19.0 * scale),
        (5.0 * scale, 15.0 * scale),
        (8.0 * scale, 23.0 * scale),
        (11.0 * scale, 22.0 * scale),
        (8.0 * scale, 14.0 * scale),
        (14.0 * scale, 14.0 * scale),
    ];

    let width = size as usize;
    let height = size as usize;
    let mut fill = vec![false; width * height];

    for y in 0..height {
        for x in 0..width {
            let inside = point_in_polygon(x as f32 + 0.5, y as f32 + 0.5, &points);
            fill[y * width + x] = inside;
        }
    }

    let mut pixels = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if !fill[idx] {
                continue;
            }

            let is_outline = neighbors(x, y, width, height)
                .any(|neighbor| !fill[neighbor]);
            let base = idx * 4;
            if is_outline {
                pixels[base] = 0xFF;
                pixels[base + 1] = 0x00;
                pixels[base + 2] = 0x00;
                pixels[base + 3] = 0x00;
            } else {
                pixels[base] = 0xFF;
                pixels[base + 1] = 0xFF;
                pixels[base + 2] = 0xFF;
                pixels[base + 3] = 0xFF;
            }
        }
    }

    CursorSprite {
        buffer: MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width as i32, height as i32),
            1,
            Transform::Normal,
            None,
        ),
        hotspot: Point::from(((scale.ceil() as i32).max(1), (scale.ceil() as i32).max(1))),
    }
}

fn point_in_polygon(x: f32, y: f32, points: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut previous = points[points.len() - 1];

    for &current in points {
        let (x1, y1) = current;
        let (x2, y2) = previous;
        let intersects = ((y1 > y) != (y2 > y))
            && (x < (x2 - x1) * (y - y1) / (y2 - y1) + x1);
        if intersects {
            inside = !inside;
        }
        previous = current;
    }

    inside
}

fn neighbors(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> impl Iterator<Item = usize> {
    let min_y = y.saturating_sub(1);
    let max_y = (y + 1).min(height - 1);
    let min_x = x.saturating_sub(1);
    let max_x = (x + 1).min(width - 1);

    (min_y..=max_y).flat_map(move |ny| (min_x..=max_x).map(move |nx| ny * width + nx))
}
