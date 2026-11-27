//! Song / artist name normalisation. Every "is this the same song?" decision goes
//! through here so that "U could" can never resolve to "U could tëll".

/// Lowercases and strips the stylised diacritics artists love ("Gët" -> "get").
pub fn fold(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'į' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ñ' | 'ń' | 'ň' => 'n',
        'ç' | 'ć' | 'č' => 'c',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

pub fn fold_str(s: &str) -> String {
    s.chars().map(fold).collect()
}

/// Folded alphanumerics only. Falls back to the folded text for all-symbol names.
pub fn norm(s: &str) -> String {
    let out: String = s.chars().map(fold).filter(|c| c.is_alphanumeric()).collect();
    if out.is_empty() {
        fold_str(s.trim())
    } else {
        out
    }
}

fn is_noise(group: &str) -> bool {
    let g = fold_str(group);
    let words: Vec<&str> = g.split(|c: char| !c.is_alphanumeric()).collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "official" | "video" | "audio" | "lyric" | "lyrics" | "visualizer" | "visualiser"
                | "prod" | "produced" | "feat" | "ft" | "featuring" | "hd" | "hq" | "4k"
                | "mv" | "directed" | "explicit"
        )
    })
}

/// Removes upload noise ("(Official Video)", "[prod. x]", "feat. y", "| Label")
/// while keeping meaningful qualifiers such as "(Remix)" or "(Outro)".
pub fn clean_title(title: &str, artist: &str) -> String {
    let mut out = String::new();
    let mut group = String::new();
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                group.push(c);
            }
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                group.push(c);
                if depth == 0 {
                    if !is_noise(&group) {
                        out.push_str(&group);
                    }
                    group.clear();
                }
            }
            _ if depth > 0 => group.push(c),
            _ => out.push(c),
        }
    }
    if !group.is_empty() && !is_noise(&group) {
        out.push_str(&group);
    }
    if let Some(i) = out.find(" | ") {
        out.truncate(i);
    }
    let lower = out.to_ascii_lowercase();
    for marker in [" feat. ", " feat ", " ft. ", " ft "] {
        if let Some(i) = lower.find(marker) {
            out.truncate(i);
            break;
        }
    }
    let mut s = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((head, tail)) = split_dash(&s) {
        if !artist.trim().is_empty() && same_artist(head, artist) && !tail.is_empty() {
            s = tail.to_string();
        }
    }
    let s = s.trim_matches(|c| c == '"' || c == '“' || c == '”').trim();
    if s.is_empty() {
        title.trim().to_string()
    } else {
        s.to_string()
    }
}

pub fn split_dash(s: &str) -> Option<(&str, &str)> {
    [" - ", " – ", " — "]
        .iter()
        .filter_map(|sep| s.find(sep).map(|i| (i, sep.len())))
        .min()
        .map(|(i, n)| (s[..i].trim(), s[i + n..].trim()))
}

pub fn title_key(title: &str, artist: &str) -> String {
    norm(&clean_title(title, artist))
}

fn artist_parts(s: &str) -> Vec<String> {
    let cleaned = crate::cache::clean_artist_name(s);
    let mut folded = fold_str(&cleaned);
    for sep in [" feat. ", " feat ", " ft. ", " ft ", " x ", " & ", " and ", "/", ";"] {
        folded = folded.replace(sep, ",");
    }
    folded
        .split(',')
        .map(norm)
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn same_artist(a: &str, b: &str) -> bool {
    let pb = artist_parts(b);
    artist_parts(a).iter().any(|p| pb.contains(p))
}

/// Turns a raw YouTube upload into (title, artist).
pub fn from_upload(raw_title: &str, uploader: &str) -> (String, String) {
    let channel = crate::cache::clean_artist_name(uploader);
    let channel = if channel == "NA" { String::new() } else { channel };
    let is_topic = uploader.trim().ends_with("Topic");
    if !is_topic {
        if let Some((head, tail)) = split_dash(raw_title) {
            if !head.is_empty() && !tail.is_empty() {
                let artist = if same_artist(head, &channel) {
                    channel.clone()
                } else {
                    head.to_string()
                };
                return (clean_title(tail, &artist), artist);
            }
        }
    }
    (clean_title(raw_title, &channel), channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_different_songs() {
        assert_ne!(title_key("U could", "Yeat"), title_key("U could tëll", "Yeat"));
    }

    #[test]
    fn upload_noise_and_accents_are_ignored() {
        assert_eq!(
            title_key("Yeat - COMË N GO (Official Music Video)", "Yeat"),
            title_key("Come N Go", "Yeat")
        );
        assert_ne!(title_key("Talk (Remix)", "Yeat"), title_key("Talk", "Yeat"));
    }

    #[test]
    fn uploads_split_into_title_and_artist() {
        assert_eq!(
            from_upload("yeat - wat ü want (prod. benjicold)", "Saint"),
            ("wat ü want".to_string(), "yeat".to_string())
        );
    }
}
