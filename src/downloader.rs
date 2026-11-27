use crate::cache::{
    add_track_to_playlist, home_dir, is_audio_file, load_and_sync_library, now_ts,
    pretty_title_from_filename, prune_lru, sanitize_slug, save_library,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    pub duration: String,
    pub uploader: String,
    pub title: String,
    pub thumbnail: String,
}

#[derive(Debug, Clone)]
pub struct SpotifyTrackSpec {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_sec: u64,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpotifyImportBundle {
    pub kind: String,
    pub collection_name: String,
    pub cover_url: Option<String>,
    pub tracks: Vec<SpotifyTrackSpec>,
}

pub fn search_youtube(query: &str) -> Result<Vec<SearchResult>, String> {
    let target = if query.starts_with("http://") || query.starts_with("https://") {
        query.to_string()
    } else {
        format!("ytsearch12:{query}")
    };
    let out = Command::new("yt-dlp")
        .args([
            "--flat-playlist",
            "--no-warnings",
            "--print",
            "%(id)s\t%(duration_string)s\t%(uploader)s\t%(title)s\t%(thumbnail)s",
            &target,
        ])
        .output()
        .map_err(|e| format!("failed to run yt-dlp: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut results = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() >= 4 {
            let id = parts[0].trim().to_string();
            let thumb = parts
                .get(4)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && *s != "NA")
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"));
            let uploader = crate::cache::clean_artist_name(parts[2]);
            results.push(SearchResult {
                id,
                duration: parts[1].trim().to_string(),
                uploader,
                title: parts[3].trim().to_string(),
                thumbnail: thumb,
            });
        }
    }
    Ok(results)
}

fn parse_spotify_kind_and_id(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    // Handle spotify:track:ID / spotify:album:ID / spotify:playlist:ID
    if let Some(rest) = trimmed.strip_prefix("spotify:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() >= 2 && matches!(parts[0], "track" | "album" | "playlist") {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }
    // Handle https://open.spotify.com/(intl-xx/)?(embed/)?(track|album|playlist)/<id>?...
    for kind in ["track", "album", "playlist"] {
        let needle = format!("/{kind}/");
        if let Some(pos) = trimmed.find(&needle) {
            let after = &trimmed[pos + needle.len()..];
            let id: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !id.is_empty() {
                return Some((kind.to_string(), id));
            }
        }
    }
    None
}

fn pick_best_spotify_cover(entity: &Value) -> Option<String> {
    if let Some(arr) = entity.pointer("/visualIdentity/image").and_then(|v| v.as_array()) {
        let best = arr
            .iter()
            .filter_map(|item| {
                let url = item.get("url")?.as_str()?;
                let w = item.get("maxWidth").and_then(|x| x.as_u64()).unwrap_or(0);
                Some((w, url.to_string()))
            })
            .max_by_key(|(w, _)| *w);
        if let Some((_, u)) = best {
            return Some(u);
        }
    }
    if let Some(arr) = entity.pointer("/coverArt/sources").and_then(|v| v.as_array()) {
        if let Some(u) = arr.first().and_then(|s| s.get("url")).and_then(|v| v.as_str()) {
            return Some(u.to_string());
        }
    }
    None
}

pub fn fetch_spotify_metadata(input: &str) -> Result<SpotifyImportBundle, String> {
    let (kind, id) = parse_spotify_kind_and_id(input)
        .ok_or_else(|| "Not a valid Spotify track, album, or playlist URL/URI".to_string())?;

    // If logged in to Spotify, use the official authenticated Web API first (supports private playlists!)
    let mut sess = crate::spotify::load_session();
    if sess.is_logged_in() {
        if let Ok(bundle) = crate::spotify::fetch_url_authenticated(&mut sess, &kind, &id) {
            if !bundle.tracks.is_empty() {
                return Ok(bundle);
            }
        }
    }
    let embed_url = format!("https://open.spotify.com/embed/{kind}/{id}");
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "12",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36",
            &embed_url,
        ])
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;

    if !out.status.success() {
        return Err("Failed to fetch Spotify embed page".into());
    }
    let html = String::from_utf8_lossy(&out.stdout);
    let marker = "<script id=\"__NEXT_DATA__\"";
    let start = html
        .find(marker)
        .ok_or("Spotify page did not include track metadata (is it private?)")?;
    let after_tag = html[start..]
        .find('>')
        .ok_or("Malformed Spotify script tag")?;
    let json_start = start + after_tag + 1;
    let json_end = html[json_start..]
        .find("</script>")
        .ok_or("Unterminated Spotify JSON")?;
    let raw_json = &html[json_start..json_start + json_end];
    let doc: Value = serde_json::from_str(raw_json).map_err(|e| format!("JSON parse error: {e}"))?;
    let ent = doc
        .pointer("/props/pageProps/state/data/entity")
        .ok_or("Missing entity in Spotify data")?;

    let col_name = ent
        .get("name")
        .or_else(|| ent.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("Spotify Import")
        .replace('\u{a0}', " ");
    let cover_url = pick_best_spotify_cover(ent);

    let mut tracks = Vec::new();
    if kind == "track" {
        let artists = ent
            .get("artists")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let dur_ms = ent.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
        tracks.push(SpotifyTrackSpec {
            title: col_name.clone(),
            artist: artists,
            album: String::new(),
            duration_sec: dur_ms / 1000,
            cover_url: cover_url.clone(),
        });
    } else if let Some(list) = ent.get("trackList").and_then(|v| v.as_array()) {
        for item in list {
            let t = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('\u{a0}', " ");
            if t.is_empty() {
                continue;
            }
            let artist = item
                .get("subtitle")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('\u{a0}', " ");
            let dur_ms = item.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
            tracks.push(SpotifyTrackSpec {
                title: t,
                artist,
                album: if kind == "album" {
                    col_name.clone()
                } else {
                    String::new()
                },
                duration_sec: dur_ms / 1000,
                cover_url: cover_url.clone(),
            });
        }
    }

    if tracks.is_empty() {
        return Err("No playable tracks found in Spotify link".into());
    }

    Ok(SpotifyImportBundle {
        kind,
        collection_name: col_name,
        cover_url,
        tracks,
    })
}

fn download_cover_image(dir: &Path, stem: &str, url: &str) -> Option<String> {
    let fname = format!("{stem}.jpg");
    let dest = dir.join(&fname);
    if dest.is_file() {
        return Some(fname);
    }
    let status = Command::new("curl")
        .args(["-sL", "--max-time", "10", "-o", dest.to_str()?, url])
        .status()
        .ok()?;
    if status.success() && fs::metadata(&dest).map(|m| m.len() > 512).unwrap_or(false) {
        Some(fname)
    } else {
        let _ = fs::remove_file(&dest);
        None
    }
}

pub fn fetch_synced_lrc(dir: &Path, stem: &str, title: &str, artist: &str, target_dur_sec: u64) -> bool {
    let lrc_path = dir.join(format!("{stem}.lrc"));
    if lrc_path.is_file() {
        return true;
    }
    let mut cleaned = title.to_string();
    for sep in ["[", "(Official", "(Lyrics", "(Audio", "|"] {
        if let Some(i) = cleaned.find(sep) {
            cleaned.truncate(i);
        }
    }
    let q_title = cleaned.trim();
    if q_title.is_empty() {
        return false;
    }
    let query = if artist.trim().is_empty() {
        q_title.to_string()
    } else {
        format!("{} {}", artist.trim(), q_title)
    };
    let output = Command::new("curl")
        .args([
            "-sG",
            "--max-time",
            "8",
            "-A",
            "itmp/0.1",
            "--data-urlencode",
            &format!("q={query}"),
            "https://lrclib.net/api/search",
        ])
        .output();
    let Ok(out) = output else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&out.stdout) else {
        return false;
    };
    let Some(arr) = val.as_array() else {
        return false;
    };
    // Pick the syncedLyrics candidate whose duration best matches our audio track
    let mut best: Option<(f64, &str)> = None;
    for item in arr {
        let Some(lrc) = item.get("syncedLyrics").and_then(|v| v.as_str()) else {
            continue;
        };
        if lrc.trim().is_empty() {
            continue;
        }
        let diff = if target_dur_sec > 0 {
            item.get("duration")
                .and_then(|d| d.as_f64())
                .map(|d| (d - target_dur_sec as f64).abs())
                .unwrap_or(999.0)
        } else {
            0.0
        };
        if best.map(|(bd, _)| diff < bd).unwrap_or(true) {
            best = Some((diff, lrc));
        }
    }
    if let Some((_, lrc)) = best {
        return fs::write(lrc_path, lrc).is_ok();
    }
    false
}

pub fn download_and_cache_track(
    dir: &Path,
    yt_url_or_search: &str,
    title: &str,
    artist: &str,
    album: &str,
    cover_url: Option<&str>,
    duration_sec: u64,
    playlist_name: Option<&str>,
    max_bytes: u64,
) -> Result<(String, String), String> {
    let slug_src = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{artist}-{title}")
    };
    let slug = sanitize_slug(&slug_src);
    let template = dir.join(format!("{slug}.%(ext)s"));

    let out = Command::new("yt-dlp")
        .args([
            "-q",
            "--no-warnings",
            "-f",
            "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
            "--print",
            "after_move:filepath",
            "--print",
            "thumbnail",
            "--print",
            "duration",
            "-o",
            template.to_str().unwrap_or("track.%(ext)s"),
            yt_url_or_search,
        ])
        .output()
        .map_err(|e| format!("failed to run yt-dlp: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout_str
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    // --print outputs in order: thumbnail, duration, after_move:filepath (after_move prints last)
    let saved_path = lines
        .iter()
        .rev()
        .find(|l| Path::new(l).is_file())
        .copied()
        .unwrap_or("");
    let yt_thumb = lines
        .iter()
        .find(|l| l.starts_with("http://") || l.starts_with("https://"))
        .copied();
    let yt_dur = lines
        .iter()
        .find_map(|l| l.parse::<f64>().ok())
        .map(|d| d as u64)
        .unwrap_or(duration_sec);

    let fname = Path::new(saved_path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or("yt-dlp did not produce an audio file")?;
    let stem = Path::new(&fname)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&slug);

    // Prefer Spotify album art if provided, else YouTube thumbnail
    let saved_cover = cover_url
        .and_then(|u| download_cover_image(dir, stem, u))
        .or_else(|| yt_thumb.and_then(|u| download_cover_image(dir, stem, u)));

    let got_lrc = fetch_synced_lrc(dir, stem, title, artist, yt_dur);

    let mut lib = load_and_sync_library(dir);
    if let Some(e) = lib.entries.get_mut(&fname) {
        e.title = title.to_string();
        if !artist.is_empty() {
            e.artist = artist.to_string();
        }
        if !album.is_empty() {
            e.album = album.to_string();
        }
        if saved_cover.is_some() {
            e.cover = saved_cover;
        }
        if yt_dur > 0 {
            e.duration_sec = yt_dur;
        }
        e.last_used = now_ts();
    }
    if let Some(pl) = playlist_name {
        add_track_to_playlist(&mut lib, pl, &fname);
    }
    let (evicted, freed) = prune_lru(dir, &mut lib, max_bytes);
    save_library(dir, &lib);

    let msg = format!(
        "Cached '{}'{}{}",
        title,
        if got_lrc { " + LRC" } else { "" },
        if evicted > 0 {
            format!(" (evicted {} old, -{:.1} MB)", evicted, freed as f64 / 1_048_576.0)
        } else {
            String::new()
        }
    );
    Ok((fname, msg))
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

/// Parses Exportify `.csv` or Spotify Account Privacy `.json` (`YourLibrary.json` / `Playlist*.json`)
/// so Free Spotify users can import Liked Songs & private playlists without Spotify Premium.
pub fn parse_spotify_export_file(path: &Path) -> Option<SpotifyImportBundle> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())?;
    let raw = fs::read_to_string(path).ok()?;
    let col_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.replace('_', " ").replace('-', " "))
        .unwrap_or_else(|| "Spotify Export".to_string());

    if ext == "csv" {
        let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
        let header: Vec<String> = parse_csv_line(lines.next()?)
            .into_iter()
            .map(|h| h.trim().trim_start_matches('\u{feff}').to_ascii_lowercase())
            .collect();
        let idx_title = header.iter().position(|h| h == "track name" || h == "title" || h == "name")?;
        let idx_artist = header
            .iter()
            .position(|h| h == "artist name(s)" || h == "artist name" || h == "artist" || h == "artists");
        let idx_album = header.iter().position(|h| h == "album name" || h == "album");
        let idx_dur = header.iter().position(|h| h.contains("duration"));

        let mut tracks = Vec::new();
        for line in lines {
            let cols = parse_csv_line(line);
            let title = cols.get(idx_title).map(|s| s.trim()).unwrap_or("");
            if title.is_empty() {
                continue;
            }
            let artist = idx_artist
                .and_then(|i| cols.get(i))
                .map(|s| s.trim().replace(';', ", "))
                .unwrap_or_default();
            let album = idx_album
                .and_then(|i| cols.get(i))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let duration_sec = idx_dur
                .and_then(|i| cols.get(i))
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|v| if v > 10_000 { v / 1000 } else { v })
                .unwrap_or(0);
            tracks.push(SpotifyTrackSpec {
                title: title.to_string(),
                artist,
                album,
                duration_sec,
                cover_url: None,
            });
        }
        if tracks.is_empty() {
            return None;
        }
        return Some(SpotifyImportBundle {
            kind: "playlist".into(),
            collection_name: col_name,
            cover_url: None,
            tracks,
        });
    }

    if ext == "json" {
        let val: Value = serde_json::from_str(&raw).ok()?;
        let mut tracks = Vec::new();
        let mut name = col_name;
        if let Some(arr) = val.get("tracks").and_then(|v| v.as_array()) {
            // Spotify YourLibrary.json
            name = "Spotify Liked Songs".into();
            for item in arr {
                if let Some(t) = item.get("track").and_then(|v| v.as_str()) {
                    tracks.push(SpotifyTrackSpec {
                        title: t.to_string(),
                        artist: item.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        album: item.get("album").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        duration_sec: 0,
                        cover_url: None,
                    });
                }
            }
        } else if let Some(pls) = val.get("playlists").and_then(|v| v.as_array()) {
            // Spotify Playlist1.json
            if let Some(first) = pls.first() {
                if let Some(n) = first.get("name").and_then(|v| v.as_str()) {
                    name = n.to_string();
                }
                if let Some(items) = first.get("items").and_then(|v| v.as_array()) {
                    for it in items {
                        if let Some(tr) = it.get("track") {
                            if let Some(t) = tr.get("trackName").and_then(|v| v.as_str()) {
                                tracks.push(SpotifyTrackSpec {
                                    title: t.to_string(),
                                    artist: tr.get("artistName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                    album: tr.get("albumName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                    duration_sec: 0,
                                    cover_url: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        if !tracks.is_empty() {
            return Some(SpotifyImportBundle {
                kind: "playlist".into(),
                collection_name: name,
                cover_url: None,
                tracks,
            });
        }
    }
    None
}

pub fn expand_local_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir().join(rest)
    } else if trimmed == "~" {
        home_dir()
    } else {
        PathBuf::from(trimmed)
    }
}

pub fn import_local_music(dir: &Path, raw_path: &str, max_bytes: u64) -> Result<String, String> {
    let src = expand_local_path(raw_path);
    if !src.exists() {
        return Err(format!("Path does not exist: {}", src.display()));
    }
    let mut files = Vec::new();
    let mut pl_name: Option<String> = None;

    if src.is_file() {
        if is_audio_file(&src)
            || src
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("wav"))
                .unwrap_or(false)
        {
            files.push(src.clone());
        } else {
            return Err("Unsupported audio format".into());
        }
    } else if src.is_dir() {
        pl_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        if let Ok(rd) = fs::read_dir(&src) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_file()
                    && (is_audio_file(&p)
                        || p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("wav"))
                            .unwrap_or(false))
                {
                    files.push(p);
                }
            }
        }
        files.sort();
    }

    if files.is_empty() {
        return Err("No audio files found at that path".into());
    }

    let mut imported = 0usize;
    let mut lib = load_and_sync_library(dir);
    for f in &files {
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
            .to_ascii_lowercase();
        let raw_stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let slug = sanitize_slug(raw_stem);
        let dest_name = format!("{slug}.{ext}");
        let dest_path = dir.join(&dest_name);
        if fs::copy(f, &dest_path).is_ok() {
            let title = pretty_title_from_filename(raw_stem);
            let _ = fetch_synced_lrc(dir, &slug, &title, "", 0);
            // Copy sibling .lrc or cover art if present
            let sib_lrc = f.with_extension("lrc");
            if sib_lrc.is_file() {
                let _ = fs::copy(&sib_lrc, dir.join(format!("{slug}.lrc")));
            }
            for cext in ["jpg", "jpeg", "png", "webp"] {
                let sib_cov = f.with_extension(cext);
                if sib_cov.is_file() {
                    let _ = fs::copy(&sib_cov, dir.join(format!("{slug}.{cext}")));
                    break;
                }
            }
            imported += 1;
            if let Some(pl) = &pl_name {
                add_track_to_playlist(&mut lib, pl, &dest_name);
            }
        }
    }
    lib = load_and_sync_library(dir);
    if let Some(pl) = &pl_name {
        for f in &files {
            let ext = f
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("mp3")
                .to_ascii_lowercase();
            let raw_stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
            let dest_name = format!("{}.{ext}", sanitize_slug(raw_stem));
            if lib.entries.contains_key(&dest_name) {
                add_track_to_playlist(&mut lib, pl, &dest_name);
            }
        }
    }
    let (evicted, freed) = prune_lru(dir, &mut lib, max_bytes);
    save_library(dir, &lib);

    let mut msg = format!("Imported {imported} local track(s)");
    if let Some(pl) = pl_name {
        msg.push_str(&format!(" into playlist '{pl}'"));
    }
    if evicted > 0 {
        msg.push_str(&format!(
            " (evicted {evicted} old, -{:.1} MB)",
            freed as f64 / 1_048_576.0
        ));
    }
    Ok(msg)
}
