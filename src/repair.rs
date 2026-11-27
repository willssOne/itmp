//! Restores titles, artists and albums that older builds lost or mangled.

use crate::{
    artist::curl_get_text,
    cache::{add_track_to_playlist, edit_library, load_and_sync_library, TrackEntry},
    downloader::{fetch_synced_lrc, SpotifyTrackSpec},
    names,
};
use serde_json::Value;
use std::{collections::HashMap, fs, path::Path, process::Command};

/// Slug rule of older builds (accented letters were dropped from filenames).
fn legacy_slug(raw: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 56 {
            break;
        }
    }
    out.trim_matches('-').to_ascii_lowercase()
}

fn stem(file: &str) -> &str {
    file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file)
}

fn is_mangled(e: &TrackEntry) -> bool {
    let artist = e.artist.trim();
    artist == "NA"
        || artist.ends_with("- Topic")
        || (names::clean_title(&e.title, artist) != e.title.trim() && e.album.is_empty())
}

fn json(url: &str) -> Option<Value> {
    serde_json::from_str(&curl_get_text(url, 8)?).ok()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Every track Deezer lists for an artist, with album names.
fn catalogue(artist: &str) -> Vec<SpotifyTrackSpec> {
    let q = encode(artist);
    let Some(found) = json(&format!("https://api.deezer.com/search/artist?q={q}&limit=5")) else {
        return Vec::new();
    };
    let Some(hit) = found["data"].as_array().and_then(|a| {
        a.iter()
            .find(|x| names::norm(x["name"].as_str().unwrap_or("")) == names::norm(artist))
    }) else {
        return Vec::new();
    };
    let Some(id) = hit["id"].as_u64() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let albums = json(&format!("https://api.deezer.com/artist/{id}/albums?limit=60"));
    for album in albums.iter().flat_map(|v| v["data"].as_array()).flatten() {
        let Some(album_id) = album["id"].as_u64() else {
            continue;
        };
        let title = album["title"].as_str().unwrap_or("").to_string();
        let cover = album["cover_big"].as_str().map(str::to_string);
        let tracks = json(&format!("https://api.deezer.com/album/{album_id}/tracks?limit=100"));
        for t in tracks.iter().flat_map(|v| v["data"].as_array()).flatten() {
            out.push(SpotifyTrackSpec {
                title: t["title"].as_str().unwrap_or("").to_string(),
                artist: t["artist"]["name"].as_str().unwrap_or(artist).to_string(),
                album: title.clone(),
                duration_sec: t["duration"].as_u64().unwrap_or(0),
                cover_url: cover.clone(),
            });
        }
    }
    out
}

/// Returns how many names and covers were fixed.
pub fn repair_library(dir: &Path) -> usize {
    repair_names(dir) + upgrade_covers(dir)
}

fn width_of(path: &Path) -> u32 {
    image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.into_dimensions().ok())
        .map(|(w, h)| w.min(h))
        .unwrap_or(0)
}

/// Replaces small cover files with the 500px album art, once per file.
fn upgrade_covers(dir: &Path) -> usize {
    const WANT: u32 = 500;
    let tried_path = dir.join(".covers_checked");
    let mut tried = fs::read_to_string(&tried_path).unwrap_or_default();
    let lib = load_and_sync_library(dir);
    let mut upgraded = 0;
    for e in lib.entries.values() {
        let Some(cover) = &e.cover else { continue };
        let path = dir.join(cover);
        let have = width_of(&path);
        if have >= WANT || e.artist.is_empty() || tried.lines().any(|l| l == cover) {
            continue;
        }
        let query = format!("{} {}", e.artist, names::clean_title(&e.title, &e.artist));
        let url = format!("https://api.deezer.com/search?q={}&limit=10", encode(&query));
        let Some(found) = json(&url).filter(|v| v["data"].is_array()) else {
            continue; // offline or rate limited: try again next launch
        };
        let want = names::title_key(&e.title, &e.artist);
        let url = found["data"].as_array().into_iter().flatten().find_map(|t| {
            let artist = t["artist"]["name"].as_str()?;
            let title = t["title"].as_str()?;
            (names::title_key(title, artist) == want && names::same_artist(artist, &e.artist))
                .then(|| t["album"]["cover_big"].as_str())
                .flatten()
        });
        if let Some(url) = url {
            let temp = path.with_extension("cover.tmp");
            let fetched = Command::new("curl")
                .args(["-sL", "--max-time", "10", "-o"])
                .arg(&temp)
                .arg(url)
                .status()
                .is_ok_and(|s| s.success());
            if fetched && width_of(&temp) > have && fs::rename(&temp, &path).is_ok() {
                upgraded += 1;
            } else {
                let _ = fs::remove_file(&temp);
            }
        }
        tried.push_str(cover);
        tried.push('\n');
    }
    let _ = fs::write(tried_path, tried);
    upgraded
}

fn repair_names(dir: &Path) -> usize {
    let lib = load_and_sync_library(dir);

    // 1. Clean up raw upload names (no network needed).
    let renames: Vec<(String, String, String)> = lib
        .entries
        .values()
        .filter(|e| is_mangled(e))
        .map(|e| {
            let (title, artist) = names::from_upload(&e.title, &e.artist);
            (e.filename.clone(), title, artist)
        })
        .collect();

    // 2. Entries with no artist at all: match their filename against the catalogue
    //    of an artist already in the library.
    let mut known: Vec<String> = lib
        .entries
        .values()
        .map(|e| e.artist.trim().to_string())
        .filter(|a| !a.is_empty() && a != "NA")
        .collect();
    known.sort();
    known.dedup();

    let mut catalogues: HashMap<String, Vec<SpotifyTrackSpec>> = HashMap::new();
    let mut found: Vec<(String, SpotifyTrackSpec)> = Vec::new();
    for e in lib.entries.values().filter(|e| e.artist.trim().is_empty()) {
        let slug = stem(&e.filename).to_ascii_lowercase();
        let Some((artist, rest)) = known
            .iter()
            .filter_map(|a| {
                let prefix = format!("{}-", legacy_slug(a));
                slug.strip_prefix(&prefix).map(|r| (a, r.to_string()))
            })
            .max_by_key(|(a, _)| a.len())
        else {
            continue;
        };
        let list = catalogues
            .entry(artist.clone())
            .or_insert_with(|| catalogue(artist));
        if list.is_empty() {
            continue; // offline: try again next launch
        }
        let spec = list
            .iter()
            .find(|t| legacy_slug(&format!("{}-{}", t.artist, t.title)) == slug)
            .or_else(|| list.iter().find(|t| legacy_slug(&t.title) == rest))
            .cloned()
            .unwrap_or_else(|| SpotifyTrackSpec {
                title: rest.replace('-', " "),
                artist: artist.clone(),
                album: String::new(),
                duration_sec: 0,
                cover_url: None,
            });
        found.push((e.filename.clone(), spec));
    }

    if renames.is_empty() && found.is_empty() {
        return 0;
    }
    edit_library(dir, |lib| {
        for (file, title, artist) in &renames {
            if let Some(e) = lib.entries.get_mut(file) {
                e.title = title.clone();
                e.artist = artist.clone();
            }
        }
        for (file, spec) in &found {
            let Some(e) = lib.entries.get_mut(file) else {
                continue;
            };
            if !e.artist.trim().is_empty() {
                continue;
            }
            e.title = spec.title.clone();
            e.artist = spec.artist.clone();
            e.album = spec.album.clone();
            if e.duration_sec == 0 {
                e.duration_sec = spec.duration_sec;
            }
            // Rejoin the album playlist the track was imported into.
            let playlist = lib
                .playlists
                .iter()
                .find(|p| !spec.album.is_empty() && p.name == spec.album)
                .map(|p| p.name.clone());
            if let Some(name) = playlist {
                add_track_to_playlist(lib, &name, file);
            }
        }
    });
    for (file, spec) in &found {
        fetch_synced_lrc(dir, stem(file), &spec.title, &spec.artist, spec.duration_sec);
    }
    renames.len() + found.len()
}
