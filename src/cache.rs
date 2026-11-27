use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_MAX_MB: u64 = 500;
pub const AUDIO_EXTS: &[&str] = &["webm", "opus", "m4a", "mp3", "ogg", "flac"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackEntry {
    pub filename: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub duration_sec: u64,
    pub size_bytes: u64,
    pub last_used: u64,
    #[serde(default, alias = "pinned")]
    pub liked: bool,
    #[serde(default)]
    pub lrc_offset_ms: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    pub tracks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LikedArtist {
    pub name: String,
    #[serde(default)]
    pub spotify_id: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub entries: HashMap<String, TrackEntry>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(default)]
    pub liked_artists: Vec<LikedArtist>,
    #[serde(default)]
    pub max_cache_mb: Option<u64>,
}

pub fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn audio_dir() -> PathBuf {
    if let Ok(custom) = env::var("ITMP_CACHE_DIR") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("itmp");
        }
    }
    home_dir().join(".local/share/itmp")
}

pub fn runtime_dir() -> PathBuf {
    PathBuf::from(env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
}

pub fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(64));
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 56 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        format!("track-{}", now_ts())
    } else {
        trimmed
    }
}

pub fn pretty_title_from_filename(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let trimmed = if let Some((head, tail)) = stem.split_once('-') {
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            tail
        } else {
            stem
        }
    } else {
        stem
    };
    trimmed.replace('-', " ").replace('_', " ")
}

fn find_cover_for_stem(dir: &Path, stem: &str) -> Option<String> {
    for ext in ["jpg", "jpeg", "png", "webp"] {
        let name = format!("{stem}.{ext}");
        if dir.join(&name).is_file() {
            return Some(name);
        }
    }
    None
}

pub fn load_and_sync_library(dir: &Path) -> Library {
    let _ = fs::create_dir_all(dir);
    let idx_path = dir.join(".cache_index.json");
    let mut lib: Library = fs::read_to_string(&idx_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut seen = HashMap::new();

    if let Ok(read) = fs::read_dir(dir) {
        for ent in read.flatten() {
            let path = ent.path();
            if !path.is_file() || !is_audio_file(&path) {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()) else {
                continue;
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&fname);
            let meta = ent.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or_else(now_ts);

            let detected_cover = find_cover_for_stem(dir, stem);

            let entry = lib
                .entries
                .remove(&fname)
                .map(|mut e| {
                    e.size_bytes = size;
                    if e.cover.as_ref().map(|c| !dir.join(c).is_file()).unwrap_or(true) {
                        e.cover = detected_cover.clone();
                    }
                    e
                })
                .unwrap_or_else(|| TrackEntry {
                    filename: fname.clone(),
                    title: pretty_title_from_filename(&fname),
                    artist: String::new(),
                    album: String::new(),
                    cover: detected_cover,
                    duration_sec: 0,
                    size_bytes: size,
                    last_used: mtime,
                    liked: false,
                    lrc_offset_ms: 0,
                });
            seen.insert(fname, entry);
        }
    }

    lib.entries = seen;

    // Prune deleted tracks out of playlists
    for pl in &mut lib.playlists {
        pl.tracks.retain(|f| lib.entries.contains_key(f));
    }

    save_library(dir, &lib);
    lib
}

pub fn save_library(dir: &Path, lib: &Library) {
    let idx_path = dir.join(".cache_index.json");
    if let Ok(json) = serde_json::to_string_pretty(lib) {
        let _ = fs::write(idx_path, json);
    }
}

pub fn total_cache_bytes(lib: &Library) -> u64 {
    lib.entries.values().map(|e| e.size_bytes).sum()
}

pub fn has_lrc(dir: &Path, filename: &str) -> bool {
    Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| dir.join(format!("{stem}.lrc")).is_file())
        .unwrap_or(false)
}

pub fn remove_track_files(dir: &Path, filename: &str) {
    let _ = fs::remove_file(dir.join(filename));
    if let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) {
        let _ = fs::remove_file(dir.join(format!("{stem}.lrc")));
        for ext in ["jpg", "jpeg", "png", "webp"] {
            let _ = fs::remove_file(dir.join(format!("{stem}.{ext}")));
        }
    }
}

pub fn prune_lru(dir: &Path, lib: &mut Library, max_bytes: u64) -> (usize, u64) {
    let mut total = total_cache_bytes(lib);
    if total <= max_bytes {
        return (0, 0);
    }
    let mut candidates: Vec<(u64, String, u64)> = lib
        .entries
        .values()
        .filter(|e| !e.liked)
        .map(|e| (e.last_used, e.filename.clone(), e.size_bytes))
        .collect();
    candidates.sort_by_key(|c| c.0);

    let mut removed = 0usize;
    let mut freed = 0u64;
    for (_, fname, sz) in candidates {
        if total <= max_bytes {
            break;
        }
        remove_track_files(dir, &fname);
        lib.entries.remove(&fname);
        for pl in &mut lib.playlists {
            pl.tracks.retain(|f| f != &fname);
        }
        total = total.saturating_sub(sz);
        removed += 1;
        freed += sz;
    }
    save_library(dir, lib);
    (removed, freed)
}

pub fn add_track_to_playlist(lib: &mut Library, playlist_name: &str, filename: &str) {
    if let Some(pl) = lib.playlists.iter_mut().find(|p| p.name.eq_ignore_ascii_case(playlist_name)) {
        if !pl.tracks.iter().any(|t| t == filename) {
            pl.tracks.push(filename.to_string());
        }
    } else {
        lib.playlists.push(Playlist {
            name: playlist_name.to_string(),
            tracks: vec![filename.to_string()],
        });
    }
}

pub fn clean_artist_name(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    for suffix in [" - Topic", " – Topic", "VEVO", " Official"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim().to_string();
        }
    }
    s
}

fn norm_match(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| match c {
            'ë' | 'Ë' => 'e',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

pub fn find_cached_track(lib: &Library, title: &str, artist: &str) -> Option<String> {
    let nt = norm_match(title);
    if nt.len() < 2 {
        return None;
    }
    let na = norm_match(&clean_artist_name(artist));
    for (fname, e) in &lib.entries {
        let et = norm_match(&e.title);
        if et == nt || (et.len() >= 4 && nt.len() >= 4 && (et.contains(&nt) || nt.contains(&et))) {
            if na.is_empty() || e.artist.is_empty() {
                return Some(fname.clone());
            }
            let ea = norm_match(&clean_artist_name(&e.artist));
            if ea.contains(&na) || na.contains(&ea) || et == nt {
                return Some(fname.clone());
            }
        }
    }
    None
}

pub fn fmt_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
