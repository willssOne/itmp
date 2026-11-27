use crate::cache::clean_artist_name;
use serde_json::Value;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct GeniusAnnotation {
    pub fragment: String,
    pub explanation: String,
}

#[derive(Debug, Clone)]
pub struct GeniusSongAnnotations {
    pub song_title: String,
    pub song_url: String,
    pub items: Vec<GeniusAnnotation>,
}

fn curl_json(url: &str) -> Option<Value> {
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "8",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/124.0.0.0 Safari/537.36",
            url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn url_enc(s: &str) -> String {
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

fn clean_title_for_search(title: &str) -> String {
    let mut s = title.to_string();
    for sep in ["[", "(Official", "(Lyrics", "(Audio", "(feat.", "(ft.", "|"] {
        if let Some(i) = s.find(sep) {
            s.truncate(i);
        }
    }
    s.trim().to_string()
}

pub fn fetch_song_annotations(title: &str, artist: &str) -> Result<GeniusSongAnnotations, String> {
    let ct = clean_title_for_search(title);
    let ca = clean_artist_name(artist);
    let q = if ca.is_empty() {
        ct.clone()
    } else {
        format!("{ca} {ct}")
    };
    if q.trim().is_empty() {
        return Err("No song metadata to search Genius".into());
    }

    let search_url = format!(
        "https://genius.com/api/search/multi?per_page=5&q={}",
        url_enc(&q)
    );
    let val = curl_json(&search_url).ok_or("Could not reach Genius API")?;

    let mut picked: Option<(u64, String, String)> = None;
    if let Some(sections) = val.pointer("/response/sections").and_then(|v| v.as_array()) {
        for sec in sections {
            let stype = sec.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if stype != "song" && stype != "top_hit" {
                continue;
            }
            if let Some(hits) = sec.get("hits").and_then(|v| v.as_array()) {
                for hit in hits {
                    if hit.get("type").and_then(|v| v.as_str()) != Some("song") {
                        continue;
                    }
                    let res = &hit["result"];
                    if let Some(id) = res.get("id").and_then(|v| v.as_u64()) {
                        let ft = res
                            .get("full_title")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&q)
                            .replace('\u{00a0}', " ");
                        let url = res
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        picked = Some((id, ft, url));
                        break;
                    }
                }
            }
            if picked.is_some() {
                break;
            }
        }
    }

    let (song_id, song_title, song_url) =
        picked.ok_or_else(|| format!("Song '{q}' not found on Genius"))?;

    let ref_url = format!(
        "https://genius.com/api/referents?song_id={song_id}&per_page=50&text_format=plain"
    );
    let refs_val = curl_json(&ref_url).ok_or("Failed to load Genius annotations")?;

    let mut items = Vec::new();
    if let Some(arr) = refs_val
        .pointer("/response/referents")
        .and_then(|v| v.as_array())
    {
        for it in arr {
            let frag = it
                .get("fragment")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let body = it
                .pointer("/annotations/0/body/plain")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !frag.is_empty() && body.len() >= 4 {
                items.push(GeniusAnnotation {
                    fragment: frag,
                    explanation: body,
                });
            }
        }
    }

    Ok(GeniusSongAnnotations {
        song_title,
        song_url,
        items,
    })
}

fn norm_words(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| match c {
            'ë' | 'Ë' => 'e',
            c if c.is_alphanumeric() || c.is_whitespace() => c.to_ascii_lowercase(),
            _ => ' ',
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_string())
        .collect()
}

pub fn match_lyric_annotation<'a>(
    song: &'a GeniusSongAnnotations,
    lyric_line: &str,
) -> Option<&'a GeniusAnnotation> {
    let line_words = norm_words(lyric_line);
    if line_words.is_empty() {
        return None;
    }
    let line_joined = line_words.join(" ");

    let mut best: Option<(f64, &'a GeniusAnnotation)> = None;
    for ann in &song.items {
        let frag_words = norm_words(&ann.fragment);
        if frag_words.is_empty() {
            continue;
        }
        let frag_joined = frag_words.join(" ");

        // Exact or substring match gets top priority
        let mut score = if frag_joined.contains(&line_joined) || line_joined.contains(&frag_joined)
        {
            1.0
        } else {
            let mut overlap = 0usize;
            for lw in &line_words {
                if frag_words.iter().any(|fw| fw == lw) {
                    overlap += 1;
                }
            }
            (overlap as f64) / (line_words.len().max(1) as f64)
        };

        // Slight bonus for longer substantive explanations over 1-word memes
        if ann.explanation.len() > 25 {
            score += 0.05;
        }

        if score >= 0.42 && best.map(|(bs, _)| score > bs).unwrap_or(true) {
            best = Some((score, ann));
        }
    }
    best.map(|(_, a)| a)
}
