use crate::cache::{runtime_dir, sanitize_slug};
use image::{imageops::FilterType, ImageReader};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub type ThumbRows = [Vec<Span<'static>>; 2];

/// Ultra-lightweight Truecolor Half-Block (`▀`) album cover / artist avatar renderer.
/// Uses ~2 KB RAM and works natively in Alacritty, Foot, Kitty, Ghostty, and tmux.
pub struct CoverArt {
    pub path_key: String,
    pub lines: Vec<Line<'static>>,
}

fn open_image_any_ext(path: &Path) -> Option<image::DynamicImage> {
    ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

impl CoverArt {
    pub fn load(path: &Path, cols: u16, rows: u16) -> Option<Self> {
        let w = (cols as u32).max(4);
        let h = ((rows as u32) * 2).max(4);
        let img = open_image_any_ext(path)?
            .resize_exact(w, h, FilterType::Triangle)
            .to_rgb8();

        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..(h / 2) {
            let mut spans = Vec::with_capacity(w as usize);
            for col in 0..w {
                let top = img.get_pixel(col, row * 2).0;
                let bot = img.get_pixel(col, row * 2 + 1).0;
                spans.push(Span::styled(
                    "▀",
                    Style::default()
                        .fg(Color::Rgb(top[0], top[1], top[2]))
                        .bg(Color::Rgb(bot[0], bot[1], bot[2])),
                ));
            }
            lines.push(Line::from(spans));
        }

        Some(Self {
            path_key: path.to_string_lossy().to_string(),
            lines,
        })
    }
}

/// Downloads a remote thumbnail/avatar image into a volatile runtime cache directory
/// (`$XDG_RUNTIME_DIR/itmp-thumbs/`) so search & artist art loads fast without bloating disk cache.
pub fn fetch_remote_image_to_temp(url: &str, key: &str) -> Option<PathBuf> {
    if url.trim().is_empty() {
        return None;
    }
    let dir = runtime_dir().join("itmp-thumbs");
    let _ = fs::create_dir_all(&dir);
    let dest = dir.join(format!("{}.img", sanitize_slug(key)));
    if dest.is_file() && fs::metadata(&dest).map(|m| m.len() > 256).unwrap_or(false) {
        return Some(dest);
    }
    let status = Command::new("curl")
        .args(["-sL", "--max-time", "6", "-o", dest.to_str()?, url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if status.success() && fs::metadata(&dest).map(|m| m.len() > 256).unwrap_or(false) {
        Some(dest)
    } else {
        let _ = fs::remove_file(&dest);
        None
    }
}

pub fn decode_thumb(path: &Path) -> Option<ThumbRows> {
    // 4 cols wide x 2 rows tall (4x4 truecolor pixels -> crisp square mini cover in monospace)
    let img = open_image_any_ext(path)?
        .resize_exact(4, 4, FilterType::Triangle)
        .to_rgb8();
    let mut r0 = Vec::with_capacity(4);
    let mut r1 = Vec::with_capacity(4);
    for col in 0..4 {
        let t0 = img.get_pixel(col, 0).0;
        let b0 = img.get_pixel(col, 1).0;
        r0.push(Span::styled(
            "▀",
            Style::default()
                .fg(Color::Rgb(t0[0], t0[1], t0[2]))
                .bg(Color::Rgb(b0[0], b0[1], b0[2])),
        ));
        let t1 = img.get_pixel(col, 2).0;
        let b1 = img.get_pixel(col, 3).0;
        r1.push(Span::styled(
            "▀",
            Style::default()
                .fg(Color::Rgb(t1[0], t1[1], t1[2]))
                .bg(Color::Rgb(b1[0], b1[1], b1[2])),
        ));
    }
    Some([r0, r1])
}

/// Tiny cached 4x2-cell (4x4 truecolor pixel) square thumbnail cache for track, search & artist rows.
#[derive(Default)]
pub struct ThumbCache {
    map: HashMap<String, Option<ThumbRows>>,
}

impl ThumbCache {
    pub fn get_or_load(&mut self, path: &Path) -> Option<&ThumbRows> {
        let key = path.to_string_lossy().to_string();
        if !self.map.contains_key(&key) {
            let loaded = decode_thumb(path);
            self.map.insert(key.clone(), loaded);
        }
        self.map.get(&key).and_then(|o| o.as_ref())
    }

    pub fn get_by_key(&self, key: &str) -> Option<&ThumbRows> {
        self.map.get(key).and_then(|o| o.as_ref())
    }

    pub fn insert(&mut self, key: String, rows: ThumbRows) {
        self.map.insert(key, Some(rows));
    }
}
