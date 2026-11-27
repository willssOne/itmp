use crate::{
    cache::{home_dir, now_ts},
    downloader::{SpotifyImportBundle, SpotifyTrackSpec},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:8888";
const SCOPES: &str =
    "user-library-read playlist-read-private playlist-read-collaborative user-read-recently-played user-top-read";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSpotifyLink {
    pub url: String,
    pub name: String,
    pub kind: String,
    pub track_count: usize,
    #[serde(default)]
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpotifySession {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub saved_links: Vec<SavedSpotifyLink>,
}

impl SpotifySession {
    pub fn is_logged_in(&self) -> bool {
        !self.client_id.is_empty() && !self.refresh_token.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum SpotifyCollectionKind {
    LikedSongs,
    RecentlyPlayed,
    TopTracks,
    Playlist { id: String },
    Album { id: String, artist: String },
    PublicUrl { url: String },
}

#[derive(Debug, Clone)]
pub struct SpotifyCollection {
    pub name: String,
    pub subtitle: String,
    pub track_count: usize,
    pub cover_url: Option<String>,
    pub kind: SpotifyCollectionKind,
}

pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("itmp/spotify.json");
        }
    }
    home_dir().join(".config/itmp/spotify.json")
}

pub fn load_session() -> SpotifySession {
    let p = config_path();
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_session(session: &SpotifySession) -> Result<(), String> {
    let p = config_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create config dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(session).map_err(|e| format!("serialize failed: {e}"))?;
    fs::write(&p, json).map_err(|e| format!("failed to write {}: {e}", p.display()))?;
    let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
    Ok(())
}

pub fn clear_session() {
    let _ = fs::remove_file(config_path());
}

pub fn open_url_in_browser(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// --- Pure-Rust SHA-256 (FIPS 180-4) & Base64URL for OAuth 2.0 PKCE (zero extra crates) ---

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn base64url_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((input.len() * 4).div_ceil(3));
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i] as u32;
        let b1 = if i + 1 < input.len() {
            input[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < input.len() {
            input[i + 2] as u32
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if i + 1 < input.len() {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        }
        if i + 2 < input.len() {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        }
        i += 3;
    }
    out
}

fn random_urlsafe(byte_len: usize) -> Result<String, String> {
    let mut buf = vec![0u8; byte_len];
    let mut f = File::open("/dev/urandom").map_err(|e| format!("urandom failed: {e}"))?;
    f.read_exact(&mut buf)
        .map_err(|e| format!("urandom read failed: {e}"))?;
    Ok(base64url_encode(&buf))
}

fn url_encode(s: &str) -> String {
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

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query_params(path_and_query: &str) -> Vec<(String, String)> {
    let Some((_, q)) = path_and_query.split_once('?') else {
        return Vec::new();
    };
    q.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

fn curl_token_request(form_fields: &[(&str, &str)]) -> Result<Value, String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        "15",
        "-X",
        "POST",
        "https://accounts.spotify.com/api/token",
        "-H",
        "Content-Type: application/x-www-form-urlencoded",
    ]);
    for (k, v) in form_fields {
        cmd.arg("--data-urlencode").arg(format!("{k}={v}"));
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let val: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("invalid token JSON from Spotify: {e}"))?;
    if let Some(err) = val.get("error").and_then(|e| e.as_str()) {
        let desc = val
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or(err);
        return Err(format!("Spotify auth error: {desc}"));
    }
    Ok(val)
}

fn spotify_api_get(access_token: &str, url: &str) -> Result<Value, String> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "15",
            "-w",
            "\n%{http_code}",
            "-H",
            &format!("Authorization: Bearer {access_token}"),
            url,
        ])
        .output()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let (body, status_code) = match raw.rsplit_once('\n') {
        Some((b, code)) => (b.trim(), code.trim().parse::<u16>().unwrap_or(0)),
        None => (raw.trim(), 0),
    };

    if status_code == 403 && body.to_ascii_lowercase().contains("premium") {
        return Err(
            "Spotify requires Premium for Developer API login. Press [p] to add any Spotify playlist/album link (works free!)"
                .into(),
        );
    }

    if let Ok(val) = serde_json::from_str::<Value>(body) {
        if let Some(err_obj) = val.get("error") {
            let status = err_obj
                .get("status")
                .and_then(|s| s.as_u64())
                .unwrap_or(status_code as u64);
            let msg = err_obj
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Spotify API error");
            return Err(format!("Spotify API {status}: {msg}"));
        }
        if status_code >= 400 {
            return Err(format!("Spotify API HTTP {status_code}: {body}"));
        }
        return Ok(val);
    }

    let snippet: String = body.chars().take(180).collect();
    if status_code >= 400 {
        Err(format!("Spotify HTTP {status_code}: {snippet}"))
    } else if snippet.is_empty() {
        Err("Empty response from Spotify API".into())
    } else {
        Err(format!("Spotify API error: {snippet}"))
    }
}

pub fn ensure_fresh_token(session: &mut SpotifySession) -> Result<String, String> {
    if !session.is_logged_in() {
        return Err("Not logged in to Spotify".into());
    }
    if !session.access_token.is_empty() && now_ts() + 60 < session.expires_at {
        return Ok(session.access_token.clone());
    }
    let val = curl_token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &session.refresh_token),
        ("client_id", &session.client_id),
    ])?;
    let access = val
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token in refresh response")?
        .to_string();
    let expires_in = val
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    if let Some(new_rt) = val.get("refresh_token").and_then(|v| v.as_str()) {
        if !new_rt.is_empty() {
            session.refresh_token = new_rt.to_string();
        }
    }
    session.access_token = access.clone();
    session.expires_at = now_ts() + expires_in;
    let _ = save_session(session);
    Ok(access)
}

/// Runs the official Spotify OAuth 2.0 Authorization Code + PKCE flow in the user's browser.
pub fn login_pkce<F>(client_id: &str, on_status: F) -> Result<SpotifySession, String>
where
    F: Fn(&str),
{
    let cid = client_id.trim();
    if cid.len() < 16 {
        return Err("Spotify Client ID looks too short (paste your 32-char Client ID from developer.spotify.com)".into());
    }

    let verifier = random_urlsafe(48)?;
    let challenge = base64url_encode(&sha256(verifier.as_bytes()));
    let state = random_urlsafe(18)?;

    let listener = TcpListener::bind(CALLBACK_ADDR).map_err(|e| {
        format!("Could not bind {CALLBACK_ADDR} for Spotify callback ({e}) - close other auth processes")
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("failed to configure listener: {e}"))?;

    let auth_url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        url_encode(cid),
        url_encode(REDIRECT_URI),
        url_encode(&challenge),
        url_encode(&state),
        url_encode(SCOPES),
    );

    open_url_in_browser(&auth_url);
    on_status("Browser opened for Spotify login -- approve access (waiting up to 3 min)...");

    let deadline = Instant::now() + Duration::from_secs(180);
    let auth_code = loop {
        if Instant::now() > deadline {
            return Err("Timed out waiting for Spotify browser login callback".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut req_buf = [0u8; 4096];
                let n = stream.read(&mut req_buf).unwrap_or(0);
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                let first_line = req_str.lines().next().unwrap_or("");
                let target = first_line.split_whitespace().nth(1).unwrap_or("");

                if !target.starts_with("/callback") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }

                let params = parse_query_params(target);
                let get_p = |k: &str| {
                    params
                        .iter()
                        .find(|(pk, _)| pk == k)
                        .map(|(_, pv)| pv.clone())
                };

                if let Some(err) = get_p("error") {
                    let body = format!(
                        "<html><body style='background:#111;color:#eee;font-family:monospace;padding:32px'><h2>Spotify login cancelled ({err})</h2><p>You can close this tab and return to itmp.</p></body></html>"
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    return Err(format!("Spotify login cancelled: {err}"));
                }

                let got_state = get_p("state").unwrap_or_default();
                let got_code = get_p("code").unwrap_or_default();
                if got_state != state || got_code.is_empty() {
                    let body = "<html><body style='background:#111;color:#eee;font-family:monospace;padding:32px'><h2>Invalid OAuth state</h2><p>Please retry login from itmp.</p></body></html>";
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    return Err("Spotify OAuth state mismatch".into());
                }

                let body = "<html><body style='background:#111;color:#8f8;font-family:monospace;padding:32px'><h2>Logged in to Spotify!</h2><p>You can close this tab and return to <b>itmp</b>.</p></body></html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                break got_code;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(80));
            }
            Err(e) => return Err(format!("callback accept error: {e}")),
        }
    };

    on_status("Exchanging Spotify token...");
    let tok = curl_token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &auth_code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", cid),
        ("code_verifier", &verifier),
    ])?;

    let access_token = tok
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?
        .to_string();
    let refresh_token = tok
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("missing refresh_token")?
        .to_string();
    let expires_in = tok
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);

    on_status("Loading Spotify profile...");
    let me = spotify_api_get(&access_token, "https://api.spotify.com/v1/me").ok();
    let display_name = me
        .as_ref()
        .and_then(|m| {
            m.get("display_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| m.get("id").and_then(|v| v.as_str()))
        })
        .unwrap_or("Spotify User")
        .to_string();

    let existing_links = load_session().saved_links;
    let session = SpotifySession {
        client_id: cid.to_string(),
        access_token,
        refresh_token,
        expires_at: now_ts() + expires_in,
        display_name,
        saved_links: existing_links,
    };
    save_session(&session)?;
    Ok(session)
}

fn parse_track_obj(t: &Value, fallback_album: &str, fallback_cover: Option<&str>) -> Option<SpotifyTrackSpec> {
    if t.is_null() {
        return None;
    }
    if t.get("is_local").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let title = t.get("name")?.as_str()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let artist = t
        .get("artists")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let album = t
        .get("album")
        .and_then(|al| al.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or(fallback_album)
        .to_string();
    let cover_url = t
        .get("album")
        .and_then(|al| al.get("images"))
        .and_then(|imgs| imgs.as_array())
        .and_then(|arr| arr.first())
        .and_then(|img| img.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| fallback_cover.map(|s| s.to_string()));
    let duration_sec = t
        .get("duration_ms")
        .and_then(|d| d.as_u64())
        .map(|ms| ms / 1000)
        .unwrap_or(0);

    Some(SpotifyTrackSpec {
        title,
        artist,
        album,
        duration_sec,
        cover_url,
    })
}

pub fn saved_links_as_collections(session: &SpotifySession) -> Vec<SpotifyCollection> {
    session
        .saved_links
        .iter()
        .map(|l| SpotifyCollection {
            name: l.name.clone(),
            subtitle: format!("Spotify {} (no Premium needed)", l.kind),
            track_count: l.track_count,
            cover_url: l.cover_url.clone(),
            kind: SpotifyCollectionKind::PublicUrl { url: l.url.clone() },
        })
        .collect()
}

pub fn fetch_collections(session: &mut SpotifySession) -> Result<Vec<SpotifyCollection>, String> {
    let mut cols = saved_links_as_collections(session);
    if !session.is_logged_in() {
        return Ok(cols);
    }
    let token = ensure_fresh_token(session)?;

    // Probe /v1/me/tracks first; if Spotify returns 403 Premium required, fail with a clear message
    // instead of showing fake/broken collections.
    let liked_probe = spotify_api_get(&token, "https://api.spotify.com/v1/me/tracks?limit=1")?;
    let liked_total = liked_probe
        .get("total")
        .and_then(|t| t.as_u64())
        .unwrap_or(0) as usize;
    cols.push(SpotifyCollection {
        name: "* Liked Songs".into(),
        subtitle: "Your saved tracks".into(),
        track_count: liked_total,
        cover_url: None,
        kind: SpotifyCollectionKind::LikedSongs,
    });

    // 2. Recently Played
    cols.push(SpotifyCollection {
        name: "Recently Played".into(),
        subtitle: "Last 50 played tracks".into(),
        track_count: 50,
        cover_url: None,
        kind: SpotifyCollectionKind::RecentlyPlayed,
    });

    // 3. Top Tracks
    cols.push(SpotifyCollection {
        name: "Top Tracks".into(),
        subtitle: "Your most played songs".into(),
        track_count: 50,
        cover_url: None,
        kind: SpotifyCollectionKind::TopTracks,
    });

    // 4. User Playlists (private + public + collaborative)
    let mut next_pl = Some("https://api.spotify.com/v1/me/playlists?limit=50".to_string());
    let mut pl_pages = 0;
    while let Some(url) = next_pl {
        if pl_pages >= 4 {
            break;
        }
        pl_pages += 1;
        let Ok(page) = spotify_api_get(&token, &url) else {
            break;
        };
        if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
            for pl in items {
                let Some(id) = pl.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let name = pl
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Playlist")
                    .to_string();
                let count = pl
                    .get("tracks")
                    .or_else(|| pl.get("items"))
                    .and_then(|t| t.get("total"))
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as usize;
                let owner = pl
                    .get("owner")
                    .and_then(|o| o.get("display_name"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("Playlist");
                let cover_url = pl
                    .get("images")
                    .and_then(|i| i.as_array())
                    .and_then(|a| a.first())
                    .and_then(|img| img.get("url"))
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
                cols.push(SpotifyCollection {
                    name,
                    subtitle: format!("Playlist by {owner}"),
                    track_count: count,
                    cover_url,
                    kind: SpotifyCollectionKind::Playlist { id: id.to_string() },
                });
            }
        }
        next_pl = page
            .get("next")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }

    // 5. Saved Albums
    if let Ok(page) = spotify_api_get(&token, "https://api.spotify.com/v1/me/albums?limit=50") {
        if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
            for item in items {
                let Some(al) = item.get("album") else {
                    continue;
                };
                let Some(id) = al.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let name = al
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Album")
                    .to_string();
                let artist = al
                    .get("artists")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let count = al
                    .get("total_tracks")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as usize;
                let cover_url = al
                    .get("images")
                    .and_then(|i| i.as_array())
                    .and_then(|a| a.first())
                    .and_then(|img| img.get("url"))
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
                cols.push(SpotifyCollection {
                    name,
                    subtitle: format!("Album - {artist}"),
                    track_count: count,
                    cover_url,
                    kind: SpotifyCollectionKind::Album {
                        id: id.to_string(),
                        artist,
                    },
                });
            }
        }
    }

    Ok(cols)
}

pub fn fetch_collection_tracks(
    session: &mut SpotifySession,
    col: &SpotifyCollection,
) -> Result<Vec<SpotifyTrackSpec>, String> {
    if let SpotifyCollectionKind::PublicUrl { url } = &col.kind {
        let bundle = crate::downloader::fetch_spotify_metadata(url)?;
        return Ok(bundle.tracks);
    }
    let token = ensure_fresh_token(session)?;
    let mut tracks = Vec::new();
    const MAX_TRACKS: usize = 300;

    match &col.kind {
        SpotifyCollectionKind::PublicUrl { .. } => unreachable!(),
        SpotifyCollectionKind::LikedSongs => {
            let mut next = Some("https://api.spotify.com/v1/me/tracks?limit=50".to_string());
            while let Some(url) = next {
                let page = spotify_api_get(&token, &url)?;
                if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
                    for it in items {
                        if let Some(tr) = it.get("track").and_then(|t| parse_track_obj(t, "", None)) {
                            tracks.push(tr);
                            if tracks.len() >= MAX_TRACKS {
                                return Ok(tracks);
                            }
                        }
                    }
                }
                next = page
                    .get("next")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
        SpotifyCollectionKind::RecentlyPlayed => {
            let page = spotify_api_get(
                &token,
                "https://api.spotify.com/v1/me/player/recently-played?limit=50",
            )?;
            let mut seen = HashSet::new();
            if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
                for it in items {
                    if let Some(tr) = it.get("track").and_then(|t| parse_track_obj(t, "", None)) {
                        let key = format!("{}|{}", tr.artist.to_lowercase(), tr.title.to_lowercase());
                        if seen.insert(key) {
                            tracks.push(tr);
                        }
                    }
                }
            }
        }
        SpotifyCollectionKind::TopTracks => {
            let page =
                spotify_api_get(&token, "https://api.spotify.com/v1/me/top/tracks?limit=50")?;
            if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
                for t in items {
                    if let Some(tr) = parse_track_obj(t, "", None) {
                        tracks.push(tr);
                    }
                }
            }
        }
        SpotifyCollectionKind::Playlist { id } => {
            let mut next = Some(format!(
                "https://api.spotify.com/v1/playlists/{id}/tracks?limit=100"
            ));
            let mut tried_items_fallback = false;
            while let Some(url) = next {
                let page = match spotify_api_get(&token, &url) {
                    Ok(p) => p,
                    Err(_) if !tried_items_fallback => {
                        tried_items_fallback = true;
                        let alt = format!(
                            "https://api.spotify.com/v1/playlists/{id}/items?limit=100"
                        );
                        spotify_api_get(&token, &alt)?
                    }
                    Err(e) => return Err(e),
                };
                if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
                    for it in items {
                        let t_obj = it.get("track").or_else(|| it.get("item"));
                        if let Some(tr) =
                            t_obj.and_then(|t| parse_track_obj(t, &col.name, col.cover_url.as_deref()))
                        {
                            tracks.push(tr);
                            if tracks.len() >= MAX_TRACKS {
                                return Ok(tracks);
                            }
                        }
                    }
                }
                next = page
                    .get("next")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
        SpotifyCollectionKind::Album { id, artist } => {
            let mut next = Some(format!(
                "https://api.spotify.com/v1/albums/{id}/tracks?limit=50"
            ));
            while let Some(url) = next {
                let page = spotify_api_get(&token, &url)?;
                if let Some(items) = page.get("items").and_then(|v| v.as_array()) {
                    for t in items {
                        if let Some(mut tr) =
                            parse_track_obj(t, &col.name, col.cover_url.as_deref())
                        {
                            if tr.artist.is_empty() {
                                tr.artist = artist.clone();
                            }
                            tracks.push(tr);
                            if tracks.len() >= MAX_TRACKS {
                                return Ok(tracks);
                            }
                        }
                    }
                }
                next = page
                    .get("next")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
    }

    Ok(tracks)
}

/// Authenticated fetch for any Spotify track / album / playlist URL (works on private playlists too!).
pub fn fetch_url_authenticated(
    session: &mut SpotifySession,
    kind: &str,
    id: &str,
) -> Result<SpotifyImportBundle, String> {
    let token = ensure_fresh_token(session)?;
    match kind {
        "track" => {
            let t = spotify_api_get(&token, &format!("https://api.spotify.com/v1/tracks/{id}"))?;
            let spec = parse_track_obj(&t, "", None).ok_or("failed to parse Spotify track")?;
            Ok(SpotifyImportBundle {
                kind: "track".into(),
                collection_name: spec.title.clone(),
                cover_url: spec.cover_url.clone(),
                tracks: vec![spec],
            })
        }
        "album" => {
            let al = spotify_api_get(&token, &format!("https://api.spotify.com/v1/albums/{id}"))?;
            let name = al
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("Spotify Album")
                .to_string();
            let artist = al
                .get("artists")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let cover_url = al
                .get("images")
                .and_then(|i| i.as_array())
                .and_then(|a| a.first())
                .and_then(|img| img.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string());
            let col = SpotifyCollection {
                name: name.clone(),
                subtitle: String::new(),
                track_count: 0,
                cover_url: cover_url.clone(),
                kind: SpotifyCollectionKind::Album {
                    id: id.to_string(),
                    artist,
                },
            };
            let tracks = fetch_collection_tracks(session, &col)?;
            Ok(SpotifyImportBundle {
                kind: "album".into(),
                collection_name: name,
                cover_url,
                tracks,
            })
        }
        "playlist" => {
            let pl =
                spotify_api_get(&token, &format!("https://api.spotify.com/v1/playlists/{id}?fields=name,images"))?;
            let name = pl
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("Spotify Playlist")
                .to_string();
            let cover_url = pl
                .get("images")
                .and_then(|i| i.as_array())
                .and_then(|a| a.first())
                .and_then(|img| img.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string());
            let col = SpotifyCollection {
                name: name.clone(),
                subtitle: String::new(),
                track_count: 0,
                cover_url: cover_url.clone(),
                kind: SpotifyCollectionKind::Playlist { id: id.to_string() },
            };
            let tracks = fetch_collection_tracks(session, &col)?;
            Ok(SpotifyImportBundle {
                kind: "playlist".into(),
                collection_name: name,
                cover_url,
                tracks,
            })
        }
        _ => Err(format!("Unsupported Spotify URL kind: {kind}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_sha256_vector_matches() {
        // Official RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let digest = sha256(verifier.as_bytes());
        assert_eq!(base64url_encode(&digest), expected_challenge);
    }
}
