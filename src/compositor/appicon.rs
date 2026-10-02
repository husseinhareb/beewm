//! Application icons for compositor chrome — currently the overview's labels.
//!
//! A window carries only a name for its application — `app_id` on Wayland, the
//! WM class on XWayland — so the icon is chased through the desktop entry
//! (`app_id.desktop` → `Icon=`) and then the XDG icon theme directories.
//!
//! XWayland windows also publish pixels directly in `_NET_WM_ICON`, but
//! `X11Surface` exposes no connection to read the property through, so they go
//! down the same desktop-entry path. That covers any app shipping an entry,
//! which in practice is all of them.
//!
//! Only PNG icons are read. Decoding SVG would pull in a rendering stack an
//! order of magnitude larger than everything here, and a window whose icon is
//! SVG-only just gets a label with no icon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use smithay::desktop::Window;

use crate::compositor::text::Rgba;

/// Icon theme directories, best first. `AdwaitaLegacy` and `hicolor` are where
/// the PNGs actually live on a modern system; `Adwaita` proper is nearly all
/// SVG and is searched only in case a PNG turns up.
const ICON_THEMES: &[&str] = &["hicolor", "AdwaitaLegacy", "Adwaita"];
/// Icon sizes to try, largest first — the label scales down, so overshooting
/// looks better than a blurry upscale.
const ICON_SIZES: &[&str] = &["64x64", "48x48", "96x96", "128x128", "32x32", "scalable"];

/// Icons already looked up, keyed by the application name and the size asked
/// for. A miss costs a `read_dir` of every applications directory plus ~57
/// `stat` calls and a PNG decode, and the overview looks up every window every
/// time it opens — which is on a key press, on the compositor's only thread.
///
/// Misses are cached as `None` too: an application with no icon is the
/// expensive case, and re-running that search on every keypress is exactly what
/// this exists to stop. Nothing here is invalidated, so an icon theme installed
/// mid-session is not picked up until restart; that is a fair trade for never
/// touching the disk on a keypress twice.
type IconCache = HashMap<(String, u32), Option<Rgba>>;
static CACHE: Mutex<Option<IconCache>> = Mutex::new(None);

/// Load the icon for `window` as a premultiplied RGBA square of `size` pixels.
/// Returns `None` when the window has no discoverable icon, which is a normal
/// outcome rather than an error.
pub fn icon_for_window(window: &Window, size: u32) -> Option<Rgba> {
    let app_id = app_id(window)?;
    let key = (app_id, size);

    if let Ok(cache) = CACHE.lock()
        && let Some(hit) = cache.as_ref().and_then(|map| map.get(&key))
    {
        return hit.clone();
    }

    let icon = lookup(&key.0, size);
    if let Ok(mut cache) = CACHE.lock() {
        cache
            .get_or_insert_with(HashMap::new)
            .insert(key, icon.clone());
    }
    icon
}

/// The uncached lookup: desktop entry, then icon theme, then decode.
fn lookup(app_id: &str, size: u32) -> Option<Rgba> {
    let icon_name = desktop_entry_icon(app_id).unwrap_or_else(|| app_id.to_string());
    let path = resolve_icon_path(&icon_name)?;
    load_png(&path, size)
}

/// The `app_id` (Wayland) or WM class (X11) identifying the application.
fn app_id(window: &Window) -> Option<String> {
    if let Some(x11) = window.x11_surface() {
        let class = x11.class();
        return (!class.is_empty()).then_some(class);
    }
    let toplevel = window.toplevel()?;
    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|role| role.app_id.clone()))
    })
    .filter(|id| !id.is_empty())
}

/// Directories holding desktop entries and icon themes, in XDG precedence
/// order: the user's own data dir first, then the system ones.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(Path::new(&home).join(".local/share"));
    }
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// The `Icon=` value from `<app_id>.desktop`, if such an entry exists.
///
/// `app_id` is matched case-insensitively against the file name, because
/// clients are inconsistent about it — Firefox reports `firefox`, its entry is
/// `firefox.desktop`, but plenty of apps differ only in case.
fn desktop_entry_icon(app_id: &str) -> Option<String> {
    let wanted = format!("{}.desktop", app_id.to_lowercase());
    for dir in data_dirs() {
        let apps = dir.join("applications");
        let exact = apps.join(format!("{app_id}.desktop"));
        let contents = std::fs::read_to_string(&exact).ok().or_else(|| {
            let entries = std::fs::read_dir(&apps).ok()?;
            entries
                .flatten()
                .find(|entry| entry.file_name().to_string_lossy().to_lowercase() == wanted)
                .and_then(|entry| std::fs::read_to_string(entry.path()).ok())
        });
        let Some(contents) = contents else {
            continue;
        };
        // Only the `[Desktop Entry]` group counts; an action group further down
        // may carry its own `Icon=` that is not the application's.
        let mut in_entry = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
            } else if in_entry && let Some(icon) = line.strip_prefix("Icon=") {
                return Some(icon.trim().to_string());
            }
        }
    }
    None
}

/// Turn an icon name into a PNG path, searching the icon themes and then the
/// legacy `pixmaps` directory. An absolute path in the desktop entry is used
/// as-is.
fn resolve_icon_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }
    for dir in data_dirs() {
        for theme in ICON_THEMES {
            for size in ICON_SIZES {
                let candidate = dir
                    .join("icons")
                    .join(theme)
                    .join(size)
                    .join("apps")
                    .join(format!("{name}.png"));
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        let pixmap = dir.join("pixmaps").join(format!("{name}.png"));
        if pixmap.exists() {
            return Some(pixmap);
        }
    }
    None
}

/// Decode a PNG into a premultiplied RGBA buffer, nearest-neighbour scaled to
/// `size` square. Nearest is enough: icons are square and the scale factor is
/// small, so the difference from a filtered downscale is not visible at label
/// size.
fn load_png(path: &Path, size: u32) -> Option<Rgba> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().ok()?;
    let mut raw = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut raw).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => return None,
    };

    let (src_w, src_h) = (info.width as usize, info.height as usize);
    let size = size.max(1) as usize;
    let mut pixels = vec![0u8; size * size * 4];
    for y in 0..size {
        let src_y = y * src_h / size;
        for x in 0..size {
            let src_x = x * src_w / size;
            let src = (src_y * src_w + src_x) * channels;
            let alpha = if channels == 4 { raw[src + 3] } else { 255 };
            let a = alpha as f32 / 255.0;
            let dst = (y * size + x) * 4;
            // Premultiply, matching what the renderer expects of a memory
            // buffer — the cursor sprites arrive the same way.
            pixels[dst] = (raw[src] as f32 * a) as u8;
            pixels[dst + 1] = (raw[src + 1] as f32 * a) as u8;
            pixels[dst + 2] = (raw[src + 2] as f32 * a) as u8;
            pixels[dst + 3] = alpha;
        }
    }

    Some(Rgba {
        width: size,
        height: size,
        pixels,
    })
}
