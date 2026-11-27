//! Pictures. Terminals that speak sixel or the kitty protocol (foot, kitty,
//! ghostty, wezterm) get real pixels; others get 2x3 "sextant" block characters.

use crate::cache::{runtime_dir, sanitize_slug};
use image::{imageops::FilterType, DynamicImage, ImageReader};
use crossterm::{cursor::MoveTo, queue, style::Print};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
    Frame,
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    Image, Resize,
};
use std::{
    collections::HashMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

enum Rendered {
    Pixels(Protocol),
    Blocks(Vec<Line<'static>>),
}

#[derive(Default)]
pub struct Art {
    picker: Option<Picker>,
    cache: HashMap<(PathBuf, u16, u16), Option<Rendered>>,
    queued: Vec<(PathBuf, Rect)>,
    /// Pixel pictures wanted in this frame, and those already on the terminal.
    placed: Vec<(PathBuf, Rect)>,
    shown: Vec<(PathBuf, Rect)>,
    screen: Rect,
}

pub fn open_image(path: &Path) -> Option<DynamicImage> {
    ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

impl Art {
    /// Asks the terminal what it can draw. Call once, in raw mode, before reading input.
    pub fn detect(background: Color) -> Self {
        let blocks_only = env::var("ITMP_GRAPHICS").is_ok_and(|v| v == "blocks")
            // Multiplexers mangle or swallow image escapes.
            || env::var_os("TMUX").is_some()
            || env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("HERDR"));
        let picker = (!blocks_only)
            .then(|| Picker::from_query_stdio().ok())
            .flatten()
            .filter(|p| p.protocol_type() != ProtocolType::Halfblocks)
            .map(|mut p| {
                if let Color::Rgb(r, g, b) = background {
                    p.set_background_color([r, g, b, 255]);
                }
                p
            });
        Art {
            picker,
            ..Art::default()
        }
    }

    pub fn describe(&self) -> &'static str {
        match self.picker.as_ref().map(Picker::protocol_type) {
            Some(ProtocolType::Sixel) => "full resolution (sixel)",
            Some(ProtocolType::Kitty) => "full resolution (kitty graphics)",
            Some(_) => "full resolution",
            None => "block characters (run itmp in foot for full resolution)",
        }
    }

    /// Ask for `path` to be drawn, square and centred, inside `area`.
    pub fn queue(&mut self, path: PathBuf, area: Rect) {
        self.queued.push((path, area));
    }

    /// Forget decoded pictures (their files changed).
    pub fn forget(&mut self) {
        self.cache.clear();
        self.shown.clear();
    }

    /// Lays out the queued pictures, skipping any hidden under a popup. Block
    /// pictures are drawn here; pixel ones only reserve their cells (see `present`).
    pub fn flush(&mut self, f: &mut Frame, covered: &[Rect]) {
        self.placed.clear();
        if self.screen != f.area() {
            // A resize wipes the terminal.
            self.screen = f.area();
            self.shown.clear();
        }
        for (path, area) in std::mem::take(&mut self.queued) {
            let area = area.intersection(f.area());
            if area.is_empty() || covered.iter().any(|c| c.intersects(area)) {
                continue;
            }
            let font = self.picker.as_ref().map(Picker::font_size);
            let (cols, rows) = square(area, font);
            if cols == 0 || rows == 0 {
                continue;
            }
            let picker = self.picker.as_ref();
            let entry = self
                .cache
                .entry((path.clone(), cols, rows))
                .or_insert_with(|| render(&path, cols, rows, picker));
            let at = Rect::new(area.x + (area.width - cols) / 2, area.y, cols, rows);
            match entry {
                Some(Rendered::Pixels(_)) => {
                    for y in at.top()..at.bottom() {
                        for x in at.left()..at.right() {
                            let cell = &mut f.buffer_mut()[(x, y)];
                            cell.reset();
                            cell.set_skip(true);
                        }
                    }
                    self.placed.push((path, at));
                }
                Some(Rendered::Blocks(lines)) => f.render_widget(Paragraph::new(lines.clone()), at),
                None => {}
            }
        }
    }

    /// Sends new pixel pictures to the terminal, after the text has been drawn.
    /// Kept outside the text buffer so unchanged pictures are never re-sent.
    pub fn present(&mut self, out: &mut impl Write) -> io::Result<()> {
        for place in &self.placed {
            if self.shown.contains(place) {
                continue;
            }
            let (path, at) = place;
            let key = (path.clone(), at.width, at.height);
            let Some(Some(Rendered::Pixels(picture))) = self.cache.get(&key) else {
                continue;
            };
            let mut scratch = Buffer::empty(*at);
            Image::new(picture).render(*at, &mut scratch);
            for y in at.top()..at.bottom() {
                for x in at.left()..at.right() {
                    let cell = &scratch[(x, y)];
                    if !cell.skip && cell.symbol() != " " {
                        queue!(out, MoveTo(x, y), Print(cell.symbol()))?;
                    }
                }
            }
        }
        self.shown.clone_from(&self.placed);
        out.flush()
    }
}

/// Largest square-looking cell rectangle that fits.
fn square(area: Rect, font: Option<(u16, u16)>) -> (u16, u16) {
    let (fw, fh) = font.unwrap_or((1, 2));
    let (fw, fh) = (fw.max(1) as u32, fh.max(1) as u32);
    let (w, h) = (area.width as u32, area.height as u32);
    let cols = (h * fh + fw / 2) / fw;
    if cols <= w {
        (cols as u16, h as u16)
    } else {
        (w as u16, ((w * fw + fh / 2) / fh).min(h) as u16)
    }
}

fn render(path: &Path, cols: u16, rows: u16, picker: Option<&Picker>) -> Option<Rendered> {
    let img = open_image(path)?;
    // Centre-crop wide video thumbnails to a square instead of squashing them.
    let side = img.width().min(img.height());
    let img = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
    match picker {
        Some(p) => {
            let (fw, fh) = p.font_size();
            let img = img.resize_to_fill(
                cols as u32 * fw as u32,
                rows as u32 * fh as u32,
                FilterType::Lanczos3,
            );
            p.new_protocol(img, Rect::new(0, 0, cols, rows), Resize::Fit(None))
                .ok()
                .map(Rendered::Pixels)
        }
        None => Some(Rendered::Blocks(
            sextant_rows(&img, cols as u32, rows as u32)
                .into_iter()
                .map(Line::from)
                .collect(),
        )),
    }
}

/// The 2x3 mosaic character for a bit mask (bit 0 = top left, bit 5 = bottom right).
fn sextant(mask: u32) -> char {
    match mask {
        0 => ' ',
        21 => '▌',
        42 => '▐',
        63 => '█',
        m => char::from_u32(0x1FB00 + m - 1 - (m > 21) as u32 - (m > 42) as u32).unwrap_or(' '),
    }
}

/// Each cell gets the two colours that best split its six pixels.
fn sextant_rows(img: &DynamicImage, cols: u32, rows: u32) -> Vec<Vec<Span<'static>>> {
    let px = img
        .resize_exact(cols * 2, rows * 3, FilterType::Lanczos3)
        .unsharpen(0.6, 2)
        .to_rgb8();
    let dist = |a: [u8; 3], b: [u8; 3]| -> i32 {
        (0..3).map(|i| (a[i] as i32 - b[i] as i32).pow(2)).sum()
    };
    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| {
                    let cell: [[u8; 3]; 6] = std::array::from_fn(|i| {
                        px.get_pixel(col * 2 + (i as u32 % 2), row * 3 + (i as u32 / 2)).0
                    });
                    // Seed with the two most different pixels, then average each side.
                    let (mut a, mut b, mut far) = (0, 0, -1);
                    for i in 0..6 {
                        for j in i + 1..6 {
                            let d = dist(cell[i], cell[j]);
                            if d > far {
                                (a, b, far) = (i, j, d);
                            }
                        }
                    }
                    let mut mask = 0u32;
                    let mut sum = [[0u32; 4]; 2];
                    for (i, p) in cell.iter().enumerate() {
                        let fg = dist(*p, cell[a]) <= dist(*p, cell[b]);
                        if fg {
                            mask |= 1 << i;
                        }
                        let side = &mut sum[fg as usize];
                        for c in 0..3 {
                            side[c] += p[c] as u32;
                        }
                        side[3] += 1;
                    }
                    let avg = |s: [u32; 4]| {
                        let n = s[3].max(1);
                        Color::Rgb((s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8)
                    };
                    let fg = avg(sum[1]);
                    let bg = if sum[0][3] == 0 { fg } else { avg(sum[0]) };
                    Span::styled(sextant(mask).to_string(), Style::default().fg(fg).bg(bg))
                })
                .collect()
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sextant_table_matches_unicode() {
        assert_eq!(sextant(1), '\u{1FB00}');
        assert_eq!(sextant(20), '\u{1FB13}');
        assert_eq!(sextant(22), '\u{1FB14}');
        assert_eq!(sextant(62), '\u{1FB3B}');
    }
}
