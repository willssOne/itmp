use image::imageops::FilterType;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::{collections::HashMap, path::Path};

/// Ultra-lightweight Truecolor Half-Block (`▀`) album cover art renderer.
/// Uses ~2 KB RAM and works natively in Alacritty, Foot, Kitty, Ghostty, and tmux.
pub struct CoverArt {
    pub path_key: String,
    pub lines: Vec<Line<'static>>,
}

impl CoverArt {
    pub fn load(path: &Path, cols: u16, rows: u16) -> Option<Self> {
        let w = (cols as u32).max(4);
        let h = ((rows as u32) * 2).max(4);
        let img = image::open(path)
            .ok()?
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

/// Tiny cached 4x2-cell (4x4 truecolor pixel) square album thumbnail for track list rows.
/// Cached by cover path so songs sharing an album cover cost ~0 extra memory.
#[derive(Default)]
pub struct ThumbCache {
    map: HashMap<String, Option<[Vec<Span<'static>>; 2]>>,
}

impl ThumbCache {
    pub fn get_or_load(&mut self, path: &Path) -> Option<&[Vec<Span<'static>>; 2]> {
        let key = path.to_string_lossy().to_string();
        if !self.map.contains_key(&key) {
            let loaded = Self::decode_thumb(path);
            self.map.insert(key.clone(), loaded);
        }
        self.map.get(&key).and_then(|o| o.as_ref())
    }

    fn decode_thumb(path: &Path) -> Option<[Vec<Span<'static>>; 2]> {
        // 4 cols wide x 2 rows tall (4x4 truecolor pixels -> crisp square mini cover in monospace!)
        let img = image::open(path)
            .ok()?
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
}
