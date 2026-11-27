//! Colours, taken from the active Omarchy theme when there is one.

use crate::cache::home_dir;
use ratatui::style::Color;
use std::{collections::HashMap, fs, path::PathBuf, time::SystemTime};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub select: Color,
    pub text: Color,
    pub bright: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub warm: Color,
    pub gold: Color,
    pub red: Color,
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim().trim_matches(|c| c == '"' || c == '\'').strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let part = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some((part(0)?, part(2)?, part(4)?))
}

pub fn mix(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t.clamp(0.0, 1.0)).round() as u8;
            Color::Rgb(f(r1, r2), f(g1, g2), f(b1, b2))
        }
        _ => b,
    }
}

fn colors_path() -> PathBuf {
    let state = home_dir().join(".local/state/omarchy/current/theme/colors.toml");
    if state.is_file() {
        state
    } else {
        home_dir().join(".config/omarchy/current/theme/colors.toml")
    }
}

impl Theme {
    pub fn stamp() -> Option<SystemTime> {
        fs::metadata(colors_path()).and_then(|m| m.modified()).ok()
    }

    pub fn load() -> Self {
        let raw = fs::read_to_string(colors_path()).unwrap_or_default();
        let map: HashMap<&str, (u8, u8, u8)> = raw
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('=')?;
                Some((k.trim(), rgb(v)?))
            })
            .collect();
        let get = |key: &str, fallback: (u8, u8, u8)| {
            let (r, g, b) = map.get(key).copied().unwrap_or(fallback);
            Color::Rgb(r, g, b)
        };
        let bg = get("background", (18, 19, 22));
        let text = get("foreground", (205, 208, 216));
        // Muted themes still need clearly readable highlights, so lift toward white.
        let lift = |c: Color, t: f64| mix(c, Color::Rgb(255, 255, 255), t);
        let accent = lift(get("accent", (118, 220, 148)), 0.22);
        Self {
            bg,
            panel: get("lighter_background", (30, 32, 38)),
            select: get("selection", (46, 50, 60)),
            text,
            bright: lift(text, 0.45),
            dim: mix(bg, text, 0.62),
            faint: mix(bg, text, 0.34),
            accent,
            warm: lift(get("yellow", (230, 160, 90)), 0.15),
            gold: lift(get("cyan", (225, 195, 100)), 0.1),
            red: lift(get("red", (220, 100, 100)), 0.2),
        }
    }
}
