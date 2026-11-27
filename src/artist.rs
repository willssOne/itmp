use crate::{
    cache::clean_artist_name,
    downloader::SpotifyTrackSpec,
};
use serde_json::Value;
use std::{collections::HashSet, process::Command};

#[derive(Debug, Clone)]
pub struct AlbumSummary {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub record_type: String,  // "album", "ep", "single", "compile"
    pub release_year: String, // "2024"
    pub track_count: usize,
    pub cover_url: Option<String>,
}

impl AlbumSummary {
    pub fn type_badge(&self) -> &'static str {
        match self.record_type.as_str() {
            "ep" => "EP",
            "single" => "SINGLE",
            "compile" => "COMPILATION",
            _ => "ALBUM",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlbumDetail {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub record_type: String,
    pub release_date: String,
    pub cover_url: Option<String>,
    pub tracks: Vec<SpotifyTrackSpec>,
}

impl AlbumDetail {
    pub fn type_badge(&self) -> &'static str {
        match self.record_type.as_str() {
            "ep" => "EP",
            "single" => "SINGLE",
            "compile" => "COMPILATION",
            _ => "ALBUM",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtistProfile {
    pub name: String,
    pub spotify_id: Option<String>,
    pub spotify_url: Option<String>,
    pub image_url: Option<String>,
    pub fans: Option<u64>,
    pub source: String,
    pub top_tracks: Vec<SpotifyTrackSpec>,
    pub albums: Vec<AlbumSummary>,
}

fn curl_get_text(url: &str, timeout_secs: u32) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            &timeout_secs.to_string(),
            "-A",
            "itmp/0.2 (https://github.com/willssOne/itmp)",
            url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn url_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn parse_spotify_artist_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("spotify:artist:") {
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    if let Some(pos) = trimmed.find("/artist/") {
        let after = &trimmed[pos + "/artist/".len()..];
        let id: String = after.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| match c {
            'ë' | 'Ë' => 'e',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn lookup_spotify_artist_id_via_musicbrainz(artist_name: &str) -> Option<String> {
    let q = url_encode_query(&format!("artist:\"{artist_name}\""));
    let search_url = format!("https://musicbrainz.org/ws/2/artist/?query={q}&fmt=json&limit=3");
    let raw = curl_get_text(&search_url, 6)?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let artists = val.get("artists")?.as_array()?;
    let target_lc = artist_name.trim().to_ascii_lowercase();

    let mut best_mbid: Option<String> = None;
    for a in artists {
        let Some(mbid) = a.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.to_ascii_lowercase() == target_lc {
            best_mbid = Some(mbid.to_string());
            break;
        }
        if best_mbid.is_none() {
            best_mbid = Some(mbid.to_string());
        }
    }
    let mbid = best_mbid?;
    let rel_url = format!("https://musicbrainz.org/ws/2/artist/{mbid}?inc=url-rels&fmt=json");
    let rel_raw = curl_get_text(&rel_url, 6)?;
    let rel_val: Value = serde_json::from_str(&rel_raw).ok()?;
    for rel in rel_val.get("relations")?.as_array()? {
        if let Some(res) = rel.pointer("/url/resource").and_then(|v| v.as_str()) {
            if let Some(sp_id) = parse_spotify_artist_id(res) {
                return Some(sp_id);
            }
        }
    }
    None
}

fn fetch_spotify_artist_embed(spotify_id: &str) -> Option<ArtistProfile> {
    let url = format!("https://open.spotify.com/embed/artist/{spotify_id}");
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "10",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36",
            &url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let html = String::from_utf8_lossy(&out.stdout);
    let marker = "<script id=\"__NEXT_DATA__\"";
    let start = html.find(marker)?;
    let gt = html[start..].find('>')? + start + 1;
    let end = html[gt..].find("</script>")? + gt;
    let data: Value = serde_json::from_str(&html[gt..end]).ok()?;
    let entity = data.pointer("/props/pageProps/state/data/entity")?;

    let name = entity
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Artist")
        .to_string();

    let image_url = entity
        .pointer("/visualIdentity/image")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let u = item.get("url")?.as_str()?;
                    let w = item.get("maxWidth").and_then(|x| x.as_u64()).unwrap_or(0);
                    Some((w, u.to_string()))
                })
                .max_by_key(|(w, _)| *w)
                .map(|(_, u)| u)
        })
        .or_else(|| {
            entity
                .pointer("/coverArt/sources/0/url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let mut tracks = Vec::new();
    if let Some(arr) = entity.get("trackList").and_then(|v| v.as_array()) {
        for item in arr {
            let Some(title) = item.get("title").and_then(|v| v.as_str()) else {
                continue;
            };
            let artist = item
                .get("subtitle")
                .and_then(|v| v.as_str())
                .unwrap_or(&name)
                .replace('\u{00a0}', " ");
            let dur_sec = item
                .get("duration")
                .and_then(|d| d.as_u64())
                .map(|ms| ms / 1000)
                .unwrap_or(0);
            tracks.push(SpotifyTrackSpec {
                title: title.to_string(),
                artist,
                album: String::new(),
                duration_sec: dur_sec,
                cover_url: image_url.clone(),
            });
        }
    }

    Some(ArtistProfile {
        name,
        spotify_id: Some(spotify_id.to_string()),
        spotify_url: Some(format!("https://open.spotify.com/artist/{spotify_id}")),
        image_url,
        fans: None,
        source: "Spotify".into(),
        top_tracks: tracks,
        albums: Vec::new(),
    })
}

struct DeezerArtistData {
    name: String,
    image_url: Option<String>,
    fans: Option<u64>,
    tracks: Vec<SpotifyTrackSpec>,
    albums: Vec<AlbumSummary>,
}

fn parse_deezer_album_summary(a: &Value, fallback_artist: &str) -> Option<AlbumSummary> {
    let id = a.get("id")?.as_u64()?;
    let title = a.get("title")?.as_str()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let artist = a
        .pointer("/artist/name")
        .and_then(|v| v.as_str())
        .unwrap_or(fallback_artist)
        .to_string();
    let record_type = a
        .get("record_type")
        .and_then(|v| v.as_str())
        .unwrap_or("album")
        .to_ascii_lowercase();
    let release_date = a.get("release_date").and_then(|v| v.as_str()).unwrap_or("");
    let release_year = release_date.chars().take(4).collect();
    let track_count = a.get("nb_tracks").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let cover_url = a
        .get("cover_big")
        .or_else(|| a.get("cover_medium"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(AlbumSummary {
        id,
        title,
        artist,
        record_type,
        release_year,
        track_count,
        cover_url,
    })
}

fn fetch_deezer_artist(artist_name: &str) -> Option<DeezerArtistData> {
    let q = url_encode_query(artist_name);
    let raw = curl_get_text(&format!("https://api.deezer.com/search/artist?q={q}&limit=5"), 6)?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let arr = val.get("data")?.as_array()?;
    let target_lc = artist_name.trim().to_ascii_lowercase();

    let picked = arr
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|v| v.as_str())
                .map(|n| n.to_ascii_lowercase() == target_lc)
                .unwrap_or(false)
        })
        .or_else(|| arr.first())?;

    let id = picked.get("id")?.as_u64()?;
    let name = picked
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(artist_name)
        .to_string();
    let fans = picked.get("nb_fan").and_then(|v| v.as_u64());
    let image_url = picked
        .get("picture_big")
        .or_else(|| picked.get("picture_medium"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Fetch Top Tracks + Discography (Albums, EPs, Singles)
    let mut tracks = Vec::new();
    if let Some(top_raw) = curl_get_text(&format!("https://api.deezer.com/artist/{id}/top?limit=15"), 6) {
        if let Ok(top_val) = serde_json::from_str::<Value>(&top_raw) {
            if let Some(items) = top_val.get("data").and_then(|v| v.as_array()) {
                for t in items {
                    let Some(title) = t.get("title").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let artist = t
                        .pointer("/artist/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&name)
                        .to_string();
                    let album = t
                        .pointer("/album/title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let cover_url = t
                        .pointer("/album/cover_medium")
                        .or_else(|| t.pointer("/album/cover_big"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let duration_sec = t.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
                    tracks.push(SpotifyTrackSpec {
                        title: title.to_string(),
                        artist,
                        album,
                        duration_sec,
                        cover_url,
                    });
                }
            }
        }
    }

    let mut albums = Vec::new();
    if let Some(alb_raw) = curl_get_text(&format!("https://api.deezer.com/artist/{id}/albums?limit=50"), 7) {
        if let Ok(alb_val) = serde_json::from_str::<Value>(&alb_raw) {
            if let Some(items) = alb_val.get("data").and_then(|v| v.as_array()) {
                let mut seen = HashSet::new();
                let mut dated: Vec<(String, AlbumSummary)> = Vec::new();
                for a in items {
                    if let Some(sum) = parse_deezer_album_summary(a, &name) {
                        let key = format!("{}|{}", norm_key(&sum.title), sum.record_type);
                        if seen.insert(key) {
                            let rd = a
                                .get("release_date")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            dated.push((rd, sum));
                        }
                    }
                }
                // Sort newest releases first (just like Spotify)
                dated.sort_by(|a, b| b.0.cmp(&a.0));
                albums = dated.into_iter().map(|(_, s)| s).collect();
            }
        }
    }

    Some(DeezerArtistData {
        name,
        image_url,
        fans,
        tracks,
        albums,
    })
}

pub fn search_albums(query: &str) -> Result<Vec<AlbumSummary>, String> {
    let q = url_encode_query(query.trim());
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let raw = curl_get_text(&format!("https://api.deezer.com/search/album?q={q}&limit=20"), 7)
        .ok_or("Album search failed")?;
    let val: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid album JSON: {e}"))?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(arr) = val.get("data").and_then(|v| v.as_array()) {
        for a in arr {
            if let Some(sum) = parse_deezer_album_summary(a, "") {
                let key = format!("{}|{}", norm_key(&sum.artist), norm_key(&sum.title));
                if seen.insert(key) {
                    out.push(sum);
                }
            }
        }
    }
    Ok(out)
}

pub fn fetch_album_detail(album_id: u64) -> Result<AlbumDetail, String> {
    let raw = curl_get_text(&format!("https://api.deezer.com/album/{album_id}"), 8)
        .ok_or("Could not load album details")?;
    let val: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid album JSON: {e}"))?;
    let title = val
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or("Album not found")?
        .to_string();
    let artist = val
        .pointer("/artist/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let record_type = val
        .get("record_type")
        .and_then(|v| v.as_str())
        .unwrap_or("album")
        .to_ascii_lowercase();
    let release_date = val
        .get("release_date")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cover_url = val
        .get("cover_big")
        .or_else(|| val.get("cover_medium"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut tracks = Vec::new();
    if let Some(arr) = val.pointer("/tracks/data").and_then(|v| v.as_array()) {
        for t in arr {
            let Some(t_title) = t.get("title").and_then(|v| v.as_str()) else {
                continue;
            };
            let t_artist = t
                .pointer("/artist/name")
                .and_then(|v| v.as_str())
                .unwrap_or(&artist)
                .to_string();
            let dur = t.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
            tracks.push(SpotifyTrackSpec {
                title: t_title.to_string(),
                artist: t_artist,
                album: title.clone(),
                duration_sec: dur,
                cover_url: cover_url.clone(),
            });
        }
    }

    Ok(AlbumDetail {
        id: album_id,
        title,
        artist,
        record_type,
        release_date,
        cover_url,
        tracks,
    })
}

pub fn find_and_fetch_album_by_name(artist: &str, album: &str) -> Result<AlbumDetail, String> {
    let q = if artist.trim().is_empty() {
        album.trim().to_string()
    } else {
        format!("{} {}", album.trim(), clean_artist_name(artist))
    };
    let results = search_albums(&q)?;
    let target_al = norm_key(album);
    let target_ar = norm_key(&clean_artist_name(artist));

    let picked = results
        .iter()
        .find(|a| {
            let ak = norm_key(&a.title);
            let ark = norm_key(&a.artist);
            (ak == target_al || ak.contains(&target_al) || target_al.contains(&ak))
                && (target_ar.is_empty() || ark.contains(&target_ar) || target_ar.contains(&ark))
        })
        .or_else(|| results.first())
        .ok_or_else(|| format!("Album '{album}' not found"))?;

    fetch_album_detail(picked.id)
}

/// Loads an Artist Profile from Spotify (official embed avatar + Top Tracks)
/// and enriches with full Discography (Albums, EPs, Singles), per-track covers & fan count.
pub fn fetch_artist_profile(
    raw_name_or_url: &str,
    known_spotify_id: Option<&str>,
) -> Result<ArtistProfile, String> {
    let direct_id = parse_spotify_artist_id(raw_name_or_url)
        .or_else(|| known_spotify_id.map(|s| s.to_string()));
    let cleaned = clean_artist_name(raw_name_or_url);
    if direct_id.is_none() && cleaned.is_empty() {
        return Err("No artist name to look up".into());
    }

    let primary_name = cleaned
        .split([',', '&'])
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(&cleaned)
        .to_string();

    let sp_id = direct_id.or_else(|| lookup_spotify_artist_id_via_musicbrainz(&primary_name));
    let mut sp_profile = sp_id.as_deref().and_then(fetch_spotify_artist_embed);

    let lookup_name = sp_profile
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| primary_name.clone());
    let dz = fetch_deezer_artist(&lookup_name);

    if let Some(ref mut sp) = sp_profile {
        if let Some(ref d) = dz {
            sp.fans = d.fans;
            sp.albums = d.albums.clone();
            if sp.image_url.is_none() {
                sp.image_url = d.image_url.clone();
            }
            for tr in &mut sp.top_tracks {
                let tk = norm_key(&tr.title);
                if let Some(m) = d.tracks.iter().find(|dt| {
                    let dk = norm_key(&dt.title);
                    !tk.is_empty() && (dk == tk || dk.contains(&tk) || tk.contains(&dk))
                }) {
                    if tr.album.is_empty() {
                        tr.album = m.album.clone();
                    }
                    if m.cover_url.is_some() {
                        tr.cover_url = m.cover_url.clone();
                    }
                }
            }
        }
        return Ok(sp.clone());
    }

    if let Some(d) = dz {
        return Ok(ArtistProfile {
            name: d.name,
            spotify_id: None,
            spotify_url: None,
            image_url: d.image_url,
            fans: d.fans,
            source: "Artist Profile".into(),
            top_tracks: d.tracks,
            albums: d.albums,
        });
    }

    Err(format!("Could not find profile for '{primary_name}'"))
}
