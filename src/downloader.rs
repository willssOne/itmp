use crate::{
    cache::{
        add_track_to_playlist, edit_library, find_cached_track, home_dir, is_audio_file,
        load_and_sync_library, now_ts, pretty_title_from_filename, prune_lru, sanitize_slug,
    },
    names,
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
    /// Cleaned artist name; `channel` is the raw uploader.
    pub uploader: String,
    pub channel: String,
    pub title: String,
    pub thumbnail: String,
}

impl SearchResult {
    pub fn seconds(&self) -> u64 {
        self.duration
            .split(':')
            .try_fold(0u64, |acc, p| p.parse::<u64>().ok().map(|n| acc * 60 + n))
            .unwrap_or(0)
    }

    /// (title, artist) as this upload should be named in the library.
    pub fn song_name(&self) -> (String, String) {
        crate::names::from_upload(&self.title, &self.channel)
    }
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
        // Channels and playlists come back without a duration; they can't be played.
        if parts.len() >= 4 && parts[1].trim() != "NA" {
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
                uploader: if uploader == "NA" { String::new() } else { uploader },
                channel: parts[2].trim().to_string(),
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

fn lrclib(endpoint: &str, params: &[(&str, &str)]) -> Option<Value> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sG", "--max-time", "8", "-A", "itmp/0.2"]);
    for (key, value) in params {
        cmd.arg("--data-urlencode").arg(format!("{key}={value}"));
    }
    let out = cmd.arg(format!("https://lrclib.net/api/{endpoint}")).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// Synced lyrics from an LRCLIB record, only if it really is this song.
fn lyrics_if_match<'a>(item: &'a Value, title: &str, artist: &str, dur: u64) -> Option<(u64, &'a str)> {
    let text = item.get("syncedLyrics")?.as_str()?;
    if text.trim().is_empty() {
        return None;
    }
    let name = item.get("trackName").and_then(|v| v.as_str())?;
    let by = item.get("artistName").and_then(|v| v.as_str()).unwrap_or("");
    if names::title_key(name, by) != names::title_key(title, artist) {
        return None;
    }
    if !artist.trim().is_empty() && !names::same_artist(by, artist) {
        return None;
    }
    let length = item.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
    let gap = if dur > 0 && length > 0 { dur.abs_diff(length) } else { 3 };
    (gap <= 8).then_some((gap, text))
}

pub fn fetch_synced_lrc(dir: &Path, stem: &str, title: &str, artist: &str, target_dur_sec: u64) -> bool {
    let lrc_path = dir.join(format!("{stem}.lrc"));
    if lrc_path.is_file() {
        return true;
    }
    let q_title = names::clean_title(title, artist);
    let q_artist = crate::cache::clean_artist_name(artist);
    let first_artist = q_artist.split(',').next().unwrap_or("").trim();
    let free = format!("{first_artist} {q_title}");
    let queries: [&[(&str, &str)]; 3] = [
        &[("track_name", &q_title), ("artist_name", first_artist)],
        &[("q", &free)],
        &[("track_name", &q_title)],
    ];
    for params in queries {
        let Some(found) = lrclib("search", params) else {
            continue;
        };
        let best = found
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| lyrics_if_match(item, title, artist, target_dur_sec))
            .min_by_key(|(gap, _)| *gap);
        if let Some((_, lrc)) = best {
            return fs::write(lrc_path, lrc).is_ok();
        }
    }
    false
}

/// Higher is better; `None` means the upload is not this song at all.
fn score_upload(
    up_title: &str,
    channel: &str,
    up_dur: u64,
    title: &str,
    artist: &str,
    dur: u64,
) -> Option<i64> {
    let want = names::title_key(title, artist);
    let (clean, up_artist) = names::from_upload(up_title, channel);
    let have = names::norm(&clean);
    let whole = names::norm(up_title);
    let mut score = if have == want || names::title_key(up_title, artist) == want {
        100
    } else if whole.contains(&want) {
        40
    } else {
        return None;
    };
    if !artist.trim().is_empty() {
        let first = names::norm(artist.split(',').next().unwrap_or(artist));
        if names::same_artist(&up_artist, artist) || names::same_artist(channel, artist) {
            score += 40;
        } else if whole.contains(&first) {
            score += 15;
        } else {
            score -= 40;
        }
    }
    if dur > 0 && up_dur > 0 {
        let gap = dur.abs_diff(up_dur) as i64;
        score += match gap {
            0..=2 => 50,
            3..=5 => 30,
            6..=12 => 5,
            _ => -gap.min(90),
        };
    }
    let upload = names::fold_str(up_title);
    let wanted = names::fold_str(title);
    for bad in [
        "live", "sped up", "speed up", "slowed", "reverb", "remix", "instrumental", "karaoke",
        "cover", "nightcore", "8d", "loop", "1 hour", "snippet", "leak", "reaction", "type beat",
        "clean", "bass boosted", "mashup", "acapella",
    ] {
        let has = |s: &str| {
            format!(" {} ", s.split(|c: char| !c.is_alphanumeric()).collect::<Vec<_>>().join(" "))
                .contains(&format!(" {bad} "))
        };
        if has(&upload) && !has(&wanted) {
            score -= 60;
        }
    }
    if channel.trim().ends_with("Topic") {
        score += 20;
    }
    if upload.contains("official audio") {
        score += 12;
    } else if upload.contains("video") {
        score -= 8;
    }
    Some(score)
}

/// Picks the upload whose name, artist and length match, instead of trusting hit #1.
fn pick_youtube(title: &str, artist: &str, dur: u64) -> Result<String, String> {
    let mut best: Option<(i64, String)> = None;
    for query in [format!("{artist} {title}"), format!("{artist} {title} audio")] {
        let hits = search_youtube(query.trim())?;
        for (rank, hit) in hits.iter().enumerate() {
            let Some(score) = score_upload(&hit.title, &hit.channel, hit.seconds(), title, artist, dur)
            else {
                continue;
            };
            let score = score - rank as i64;
            if best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
                best = Some((score, hit.id.clone()));
            }
        }
        if best.as_ref().map(|(s, _)| *s >= 150).unwrap_or(false) {
            break;
        }
    }
    best.filter(|(s, _)| *s > 40)
        .map(|(_, id)| format!("https://www.youtube.com/watch?v={id}"))
        .ok_or_else(|| format!("no upload on YouTube matches '{artist} - {title}'"))
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
    let video_id = yt_url_or_search
        .split_once("watch?v=")
        .map(|(_, id)| id.to_string());
    let lib = load_and_sync_library(dir);
    if let Some(have) = find_cached_track(&lib, title, artist, duration_sec, video_id.as_deref()) {
        if let Some(pl) = playlist_name {
            edit_library(dir, |lib| add_track_to_playlist(lib, pl, &have));
        }
        return Ok((have, format!("'{title}' is already downloaded")));
    }

    let source = if yt_url_or_search.starts_with("ytsearch") {
        pick_youtube(title, artist, duration_sec)?
    } else {
        yt_url_or_search.to_string()
    };
    let slug_src = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{artist}-{title}")
    };
    // The video id keeps two songs with look-alike names from sharing files.
    let slug = sanitize_slug(&slug_src);
    let template = dir.join(format!("{slug}-%(id)s.%(ext)s"));

    let out = Command::new("yt-dlp")
        .args([
            "-q",
            "--no-warnings",
            "--no-playlist",
            "-f",
            "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
            "--print",
            "after_move:filepath",
            "--print",
            "thumbnail",
            "--print",
            "duration",
            "--print",
            "ID=%(id)s",
            "-o",
            template.to_str().unwrap_or("track-%(id)s.%(ext)s"),
            &source,
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
    let source_id = lines
        .iter()
        .find_map(|l| l.strip_prefix("ID="))
        .map(str::to_string);

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

    let (_, (evicted, freed)) = edit_library(dir, |lib| {
        if let Some(e) = lib.entries.get_mut(&fname) {
            e.title = title.to_string();
            e.artist = artist.to_string();
            e.album = album.to_string();
            if saved_cover.is_some() {
                e.cover = saved_cover;
            }
            if yt_dur > 0 {
                e.duration_sec = yt_dur;
            }
            e.source_id = source_id;
            e.last_used = now_ts();
        }
        if let Some(pl) = playlist_name {
            add_track_to_playlist(lib, pl, &fname);
        }
        prune_lru(dir, lib, max_bytes)
    });

    let msg = format!(
        "Downloaded '{}'{}{}",
        title,
        if got_lrc { " with lyrics" } else { "" },
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

    let mut copied = Vec::new();
    for f in &files {
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
            .to_ascii_lowercase();
        let raw_stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let slug = sanitize_slug(raw_stem);
        let dest_name = format!("{slug}.{ext}");
        if fs::copy(f, dir.join(&dest_name)).is_ok() {
            // Copy sibling .lrc or cover art if present, else look lyrics up
            let sib_lrc = f.with_extension("lrc");
            if sib_lrc.is_file() {
                let _ = fs::copy(&sib_lrc, dir.join(format!("{slug}.lrc")));
            } else {
                let title = pretty_title_from_filename(raw_stem);
                let _ = fetch_synced_lrc(dir, &slug, &title, "", 0);
            }
            for cext in ["jpg", "jpeg", "png", "webp"] {
                let sib_cov = f.with_extension(cext);
                if sib_cov.is_file() {
                    let _ = fs::copy(&sib_cov, dir.join(format!("{slug}.{cext}")));
                    break;
                }
            }
            copied.push(dest_name);
        }
    }
    let imported = copied.len();
    let (_, (evicted, freed)) = edit_library(dir, |lib| {
        if let Some(pl) = &pl_name {
            for name in &copied {
                if lib.entries.contains_key(name) {
                    add_track_to_playlist(lib, pl, name);
                }
            }
        }
        prune_lru(dir, lib, max_bytes)
    });

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
