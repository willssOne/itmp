use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct LrcWord {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub is_adlib: bool,
}

#[derive(Debug, Clone)]
pub struct LrcLine {
    pub time_sec: f64,
    pub text: String,
    pub words: Vec<LrcWord>,
}

/// Compact 20ms (50 Hz) vocal-band (300-1900 Hz) energy & onset envelope.
/// ~40 KB per 3-minute track; strips 808 sub-bass and hi-hats to track vocals.
#[derive(Debug, Clone)]
pub struct AudioEnvelope {
    pub frame_sec: f64,
    pub rms: Vec<f32>,
    pub onset: Vec<f32>,
}

impl AudioEnvelope {
    pub fn extract(audio_path: &Path) -> Option<Self> {
        let out = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                audio_path.to_str()?,
                "-ac",
                "1",
                "-ar",
                "4000",
                "-af",
                "highpass=f=300,lowpass=f=1900",
                "-f",
                "s16le",
                "-",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !out.status.success() || out.stdout.len() < 320 {
            return None;
        }

        // 80 samples at 4000 Hz = 20ms per frame
        const SAMPLES_PER_FRAME: usize = 80;
        const BYTES_PER_FRAME: usize = SAMPLES_PER_FRAME * 2;
        let frame_count = out.stdout.len() / BYTES_PER_FRAME;
        if frame_count < 10 {
            return None;
        }

        let mut raw_rms = Vec::with_capacity(frame_count);
        for f in 0..frame_count {
            let base = f * BYTES_PER_FRAME;
            let mut sum_sq = 0.0f64;
            for s in 0..SAMPLES_PER_FRAME {
                let idx = base + s * 2;
                let sample = i16::from_le_bytes([out.stdout[idx], out.stdout[idx + 1]]) as f64
                    / 32768.0;
                sum_sq += sample * sample;
            }
            raw_rms.push((sum_sq / SAMPLES_PER_FRAME as f64).sqrt() as f32);
        }

        // Light 3-frame smoothing so single-frame spikes don't jitter timing
        let mut rms = Vec::with_capacity(frame_count);
        for i in 0..frame_count {
            let a = raw_rms[i.saturating_sub(1)];
            let b = raw_rms[i];
            let c = raw_rms[(i + 1).min(frame_count - 1)];
            rms.push(0.25 * a + 0.50 * b + 0.25 * c);
        }

        let mut onset = Vec::with_capacity(frame_count);
        onset.push(0.0);
        for i in 1..frame_count {
            onset.push((rms[i] - rms[i - 1]).max(0.0));
        }

        Some(Self {
            frame_sec: 0.02,
            rms,
            onset,
        })
    }

    fn time_to_idx(&self, t: f64) -> usize {
        if self.rms.is_empty() {
            return 0;
        }
        ((t.max(0.0) / self.frame_sec).floor() as usize).min(self.rms.len() - 1)
    }

    /// Detects the real active vocal interval inside `[t_start, t_end]`
    /// and returns `(v_start, v_end, vocal_tail_end)`.
    /// `vocal_tail_end` captures trailing adlib vocal energy past the main phrase.
    fn detect_vocal_bounds(
        &self,
        t_start: f64,
        t_end: f64,
        nat_min: f64,
        nat_max: f64,
    ) -> (f64, f64, f64) {
        let i0 = self.time_to_idx(t_start);
        let i1 = self.time_to_idx((t_end - 0.03).max(t_start + 0.25));
        if i1 <= i0 + 4 {
            let ve = (t_start + nat_max).min(t_end - 0.04).max(t_start + 0.25);
            return (t_start, ve, ve);
        }

        let slice = &self.rms[i0..=i1];
        let mut sorted: Vec<f32> = slice.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let floor = sorted[sorted.len() / 6];
        let peak = sorted[(sorted.len() * 9) / 10].max(floor + 1e-4);
        let range = (peak - floor).max(1e-4);
        let active_thresh = floor + 0.22 * range;

        // Trim dead lead-in up to +0.28s if the LRC timestamp came slightly early
        let max_lead_frames = ((0.28 / self.frame_sec) as usize).min(slice.len() / 4);
        let mut lead_off = 0usize;
        for (k, &v) in slice.iter().take(max_lead_frames).enumerate() {
            if v >= active_thresh {
                lead_off = k;
                break;
            }
        }
        let v_start = t_start + (lead_off as f64) * self.frame_sec;

        // Find last active vocal frame in the full line window (captures adlib echoes too)
        let mut last_active_full = slice.len() - 1;
        for k in (lead_off..slice.len()).rev() {
            if slice[k] >= floor + 0.18 * range {
                last_active_full = k;
                break;
            }
        }
        let vocal_tail_end = (t_start + (last_active_full as f64 + 1.0) * self.frame_sec)
            .min(t_end - 0.03)
            .max(v_start + 0.25);

        // Main vocal end: respect natural vocal rate cap so short bars don't crawl across beat pockets
        let cap_end = (v_start + nat_max).min(vocal_tail_end);
        let cap_rel = ((cap_end - t_start) / self.frame_sec)
            .floor()
            .clamp(lead_off as f64, (slice.len() - 1) as f64) as usize;
        let mut last_main = cap_rel;
        for k in (lead_off..=cap_rel).rev() {
            if slice[k] >= active_thresh {
                last_main = k;
                break;
            }
        }
        let detected_main_end = t_start + (last_main as f64 + 1.0) * self.frame_sec;
        let min_end = (v_start + nat_min).min(t_end - 0.04);
        let v_end = detected_main_end
            .max(min_end)
            .min(cap_end.max(min_end))
            .min(t_end - 0.04);

        (v_start, v_end, vocal_tail_end.max(v_end))
    }

    /// Builds a normalized cumulative vocal activity curve over `[start_t, end_t]`
    /// so dense syllables move faster and breath pauses naturally slow down.
    fn build_cdf(&self, start_t: f64, end_t: f64) -> Option<(usize, Vec<f64>)> {
        let i0 = self.time_to_idx(start_t);
        let i1 = self.time_to_idx(end_t);
        if i1 <= i0 + 3 {
            return None;
        }
        let n = i1 - i0 + 1;
        let mut max_rms = 1e-4f32;
        let mut max_ons = 1e-4f32;
        for i in i0..=i1 {
            max_rms = max_rms.max(self.rms[i]);
            max_ons = max_ons.max(self.onset[i]);
        }

        let mut cdf = Vec::with_capacity(n + 1);
        cdf.push(0.0);
        let mut acc = 0.0f64;
        for i in i0..=i1 {
            let nr = (self.rms[i] / max_rms) as f64;
            let no = (self.onset[i] / max_ons) as f64;
            // 0.28 floor prevents stalling in brief dips while letting real vocal bursts drive pace
            let act = 0.28 + 0.44 * nr + 0.28 * no;
            acc += act;
            cdf.push(acc);
        }
        if acc <= 1e-6 {
            return None;
        }
        for v in &mut cdf {
            *v /= acc;
        }
        Some((i0, cdf))
    }

    /// Maps a normalized linguistic progress `u in [0, 1]` to a timestamp in `[start_t, end_t]`
    /// blended 50/50 between uniform linguistic time and audio-envelope CDF time.
    fn warp_time(&self, start_t: f64, end_t: f64, cdf_opt: Option<&(usize, Vec<f64>)>, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        let lin = start_t + u * (end_t - start_t);
        let Some((i0, cdf)) = cdf_opt else {
            return lin;
        };
        let n = cdf.len().saturating_sub(1);
        if n == 0 {
            return lin;
        }
        // Binary search in CDF for `u`
        let pos = match cdf.binary_search_by(|v| v.total_cmp(&u)) {
            Ok(idx) => idx as f64,
            Err(idx) => {
                if idx == 0 {
                    0.0
                } else if idx >= cdf.len() {
                    n as f64
                } else {
                    let lo = cdf[idx - 1];
                    let hi = cdf[idx];
                    let frac = if hi > lo { (u - lo) / (hi - lo) } else { 0.0 };
                    (idx - 1) as f64 + frac
                }
            }
        };
        let audio_t = ((*i0 as f64) + pos) * self.frame_sec;
        let audio_clamped = audio_t.clamp(start_t, end_t);
        0.48 * lin + 0.52 * audio_clamped
    }
}

#[derive(Debug, Clone)]
enum LineToken {
    Main(String),
    Adlib { text: String, group_id: usize },
}

struct AdlibGroup {
    /// Index into the main-word array immediately preceding this adlib (or None if before first main word)
    anchor_main_idx: Option<usize>,
    /// True if no main words appear after this adlib group
    is_trailing: bool,
    word_indices: Vec<usize>, // indices into the final line words Vec
}

fn count_syllables(word: &str) -> usize {
    let clean: String = word
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || *c == 'ë' || *c == 'Ë')
        .map(|c| match c {
            'ë' | 'Ë' => 'e',
            other => other.to_ascii_lowercase(),
        })
        .collect();
    if clean.is_empty() {
        return 1;
    }
    let is_vowel = |b: u8| matches!(b, b'a' | b'e' | b'i' | b'o' | b'u' | b'y');
    let bytes = clean.as_bytes();
    let mut cnt = 0usize;
    let mut prev_v = false;
    for &b in bytes {
        let v = is_vowel(b);
        if v && !prev_v {
            cnt += 1;
        }
        prev_v = v;
    }
    if clean.ends_with('e') && cnt > 1 && !clean.ends_with("le") && !clean.ends_with("ye") {
        cnt -= 1;
    }
    cnt.max(1)
}

/// Tokenizes a lyric line into main words and parenthesized adlib groups `(...)`,
/// preserving exact visual order on the same line.
fn split_main_and_adlibs(line_text: &str) -> Vec<LineToken> {
    let raw: Vec<&str> = line_text.split_whitespace().collect();
    let mut tokens = Vec::with_capacity(raw.len());
    let mut in_paren = false;
    let mut group_id = 0usize;
    let mut has_main = false;

    for w in &raw {
        let starts_p = w.starts_with('(');
        let ends_p = w.ends_with(')') || w.ends_with("),") || w.ends_with(").") || w.ends_with(")?") || w.ends_with(")!");
        if starts_p && !in_paren {
            in_paren = true;
            tokens.push(LineToken::Adlib {
                text: (*w).to_string(),
                group_id,
            });
            if ends_p {
                in_paren = false;
                group_id += 1;
            }
        } else if in_paren {
            tokens.push(LineToken::Adlib {
                text: (*w).to_string(),
                group_id,
            });
            if ends_p {
                in_paren = false;
                group_id += 1;
            }
        } else {
            has_main = true;
            tokens.push(LineToken::Main((*w).to_string()));
        }
    }

    // If the entire line is parenthesized (e.g. producer tag "(Swiz need to pipe down)"),
    // treat it as main vocal so it highlights across the bar instead of having no anchor.
    if !has_main {
        return raw
            .into_iter()
            .map(|w| LineToken::Main(w.to_string()))
            .collect();
    }
    tokens
}

fn word_weight_and_pause(w: &str, is_last: bool) -> (f64, f64, usize) {
    let syl = count_syllables(w);
    let alpha_len = w
        .chars()
        .filter(|c| c.is_alphanumeric())
        .count()
        .max(1) as f64;
    let wt = 0.68 * (syl as f64) + 0.32 * (alpha_len / 3.6).max(0.9);
    let pause = if is_last {
        0.0
    } else if w.ends_with(',') || w.ends_with(';') {
        0.42
    } else if w.ends_with('.') || w.ends_with('!') || w.ends_with('?') {
        0.58
    } else {
        0.07
    };
    (wt, pause, syl)
}

fn assign_sequence_timings(
    words: &[String],
    start_t: f64,
    end_t: f64,
    env: Option<&AudioEnvelope>,
) -> Vec<(f64, f64)> {
    let n = words.len();
    if n == 0 {
        return Vec::new();
    }
    let span = (end_t - start_t).max(0.18);
    let mut weights = Vec::with_capacity(n);
    let mut pauses = Vec::with_capacity(n);
    for (i, w) in words.iter().enumerate() {
        let (wt, p, _) = word_weight_and_pause(w, i + 1 == n);
        weights.push(wt);
        pauses.push(p);
    }
    let total: f64 = (weights.iter().sum::<f64>() + pauses.iter().sum::<f64>()).max(1e-4);

    let cdf_opt = env.and_then(|e| e.build_cdf(start_t, start_t + span));

    let mut out = Vec::with_capacity(n);
    let mut acc = 0.0f64;
    let mut prev_end = start_t;
    for i in 0..n {
        let u0 = acc / total;
        let u1 = (acc + weights[i]) / total;
        acc += weights[i] + pauses[i];

        let mut ws = match env {
            Some(e) => e.warp_time(start_t, start_t + span, cdf_opt.as_ref(), u0),
            None => start_t + u0 * span,
        };
        let mut we = match env {
            Some(e) => e.warp_time(start_t, start_t + span, cdf_opt.as_ref(), u1),
            None => start_t + u1 * span,
        };
        ws = ws.max(prev_end);
        we = we.max(ws + 0.08).min(start_t + span);
        if we <= ws {
            we = ws + 0.08;
        }
        out.push((ws, we));
        prev_end = we;
    }
    out
}

/// Parses inline enhanced LRC word tags like `<00:12.34>word` if present.
fn parse_enhanced_lrc_words(text: &str, t_end: f64) -> Option<Vec<LrcWord>> {
    if !text.contains('<') || !text.contains('>') {
        return None;
    }
    let mut tagged: Vec<(f64, String)> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            break;
        };
        let ts = &after[..close];
        let after_tag = &after[close + 1..];
        let next_open = after_tag.find('<').unwrap_or(after_tag.len());
        let chunk = after_tag[..next_open].trim();
        if let Some((mm, ss)) = ts.split_once(':') {
            if let (Ok(m), Ok(sec)) = (mm.parse::<u64>(), ss.parse::<f64>()) {
                let t = (m as f64) * 60.0 + sec;
                for w in chunk.split_whitespace() {
                    tagged.push((t, w.to_string()));
                }
            }
        }
        rest = &after_tag[next_open..];
    }
    if tagged.is_empty() {
        return None;
    }
    let mut in_paren = false;
    let mut out = Vec::with_capacity(tagged.len());
    for i in 0..tagged.len() {
        let (t0, ref w) = tagged[i];
        let t1 = if i + 1 < tagged.len() {
            tagged[i + 1].0.max(t0 + 0.08)
        } else {
            t_end.max(t0 + 0.25)
        };
        let is_adlib = if w.starts_with('(') {
            in_paren = !w.ends_with(')');
            true
        } else if in_paren {
            if w.ends_with(')') {
                in_paren = false;
            }
            true
        } else {
            false
        };
        out.push(LrcWord {
            text: w.clone(),
            start: t0,
            end: t1,
            is_adlib,
        });
    }
    Some(out)
}

fn build_word_timings(
    line_text: &str,
    t_start: f64,
    t_end: f64,
    env: Option<&AudioEnvelope>,
) -> Vec<LrcWord> {
    if let Some(enhanced) = parse_enhanced_lrc_words(line_text, t_end) {
        return enhanced;
    }

    let tokens = split_main_and_adlibs(line_text);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut main_texts: Vec<String> = Vec::new();
    let mut main_word_slots: Vec<usize> = Vec::new(); // final index in `out`
    let mut groups: Vec<AdlibGroup> = Vec::new();
    let mut group_map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    let mut out: Vec<LrcWord> = Vec::with_capacity(tokens.len());

    for tok in &tokens {
        match tok {
            LineToken::Main(w) => {
                let slot = out.len();
                main_texts.push(w.clone());
                main_word_slots.push(slot);
                out.push(LrcWord {
                    text: w.clone(),
                    start: t_start,
                    end: t_start + 0.15,
                    is_adlib: false,
                });
            }
            LineToken::Adlib { text, group_id } => {
                let slot = out.len();
                let anchor = if main_texts.is_empty() {
                    None
                } else {
                    Some(main_texts.len() - 1)
                };
                let gi = *group_map.entry(*group_id).or_insert_with(|| {
                    let id = groups.len();
                    groups.push(AdlibGroup {
                        anchor_main_idx: anchor,
                        is_trailing: false,
                        word_indices: Vec::new(),
                    });
                    id
                });
                groups[gi].word_indices.push(slot);
                out.push(LrcWord {
                    text: text.clone(),
                    start: t_start,
                    end: t_start + 0.15,
                    is_adlib: true,
                });
            }
        }
    }

    let total_main = main_texts.len();
    for g in &mut groups {
        g.is_trailing = g.anchor_main_idx.map(|a| a + 1 >= total_main).unwrap_or(false);
    }

    // 1. Estimate realistic natural duration bounds for the MAIN vocal ONLY
    //    (Adlibs NEVER steal time from the main lyrics!)
    let main_syls: usize = main_texts.iter().map(|w| count_syllables(w)).sum();
    let main_pauses: f64 = main_texts
        .iter()
        .enumerate()
        .map(|(i, w)| word_weight_and_pause(w, i + 1 == total_main).1)
        .sum();
    let span = (t_end - t_start).clamp(0.30, 8.5);
    // Natural rap/sung pace: ~0.12s/syl fast, ~0.34s/syl relaxed (prevents slow-mo stretching!)
    let nat_min = ((main_syls as f64) * 0.12 + main_pauses * 0.12)
        .clamp(0.30, (span * 0.85).max(0.30));
    let nat_max = ((main_syls as f64) * 0.34 + main_pauses * 0.28 + 0.25)
        .clamp(nat_min + 0.15, (span * 0.94).max(nat_min + 0.15));

    let (v_start, v_end, vocal_tail_end) = match env {
        Some(e) => e.detect_vocal_bounds(t_start, t_start + span, nat_min, nat_max),
        None => {
            let ve = (t_start + nat_max.min(span * 0.90)).max(t_start + nat_min);
            (t_start, ve, (t_start + span * 0.94).max(ve))
        }
    };

    // 2. Time all MAIN words inside `[v_start, v_end]`
    let main_timings = assign_sequence_timings(&main_texts, v_start, v_end, env);
    for (&slot, &(ws, we)) in main_word_slots.iter().zip(main_timings.iter()) {
        out[slot].start = ws;
        out[slot].end = we;
    }

    // 3. Time each ADLIB group IN PARALLEL on the SAME LINE so adlibs play simultaneously!
    for g in &groups {
        let adlib_texts: Vec<String> = g.word_indices.iter().map(|&idx| out[idx].text.clone()).collect();
        let adlib_syls: usize = adlib_texts.iter().map(|w| count_syllables(w)).sum();
        let adlib_pauses: f64 = adlib_texts
            .iter()
            .enumerate()
            .map(|(i, w)| word_weight_and_pause(w, i + 1 == adlib_texts.len()).1)
            .sum();
        let nat_adlib_dur = ((adlib_syls as f64) * 0.22 + adlib_pauses * 0.18 + 0.16).clamp(0.32, 2.4);

        let (a_start, a_end) = if g.is_trailing {
            // Trailing adlib (e.g. "(pop out)", "(fuck that, pop out)", "(go, go)"):
            // Overlaps the tail of the main vocal so BOTH light up at the same time on the same line,
            // then rides cleanly into the vocal echo tail!
            let overlap_words = if adlib_texts.len() >= 3 { 3 } else { 2 };
            let overlap_idx = total_main.saturating_sub(overlap_words);
            let overlap_t = main_timings
                .get(overlap_idx)
                .map(|&(s, _)| s)
                .unwrap_or(v_start);
            let min_start = (v_start + (v_end - v_start) * 0.45).min(v_end - 0.15);
            let a_s = overlap_t.max(min_start).max(v_start);
            let target_end = (a_s + nat_adlib_dur).max(vocal_tail_end.min(a_s + nat_adlib_dur * 1.35));
            let a_e = target_end.min(t_start + span - 0.03).max(a_s + 0.28);
            (a_s, a_e)
        } else if let Some(anchor) = g.anchor_main_idx {
            // Mid-line adlib (e.g. "I can't choose (can't choose), I can't decide..."):
            // Starts right as the anchor phrase lands and runs IN PARALLEL with the next main words!
            let anchor_end = main_timings
                .get(anchor)
                .map(|&(_, e)| e)
                .unwrap_or(v_start);
            let a_s = (anchor_end - 0.06).max(v_start);
            let a_e = (a_s + nat_adlib_dur)
                .min(t_start + span - 0.03)
                .max(a_s + 0.28);
            (a_s, a_e)
        } else {
            // Leading adlib at the very start of a line
            let a_s = v_start;
            let a_e = (a_s + nat_adlib_dur).min(v_end).max(a_s + 0.28);
            (a_s, a_e)
        };

        let a_timings = assign_sequence_timings(&adlib_texts, a_start, a_end, env);
        for (&slot, &(ws, we)) in g.word_indices.iter().zip(a_timings.iter()) {
            out[slot].start = ws;
            out[slot].end = we;
        }
    }

    out
}

pub fn load_lrc(dir: &Path, filename: &str, env: Option<&AudioEnvelope>) -> Vec<LrcLine> {
    let Some(stem) = Path::new(filename).file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(dir.join(format!("{stem}.lrc"))) else {
        return Vec::new();
    };
    let mut raw_lines: Vec<(f64, String)> = Vec::new();
    for line in raw.lines() {
        let s = line.trim();
        if !s.starts_with('[') {
            continue;
        }
        // Support multiple timestamps on one line like [00:12.00][01:02.00]Chorus
        let mut cursor = s;
        let mut timestamps = Vec::new();
        while cursor.starts_with('[') {
            let Some(end_br) = cursor.find(']') else {
                break;
            };
            let ts = &cursor[1..end_br];
            if let Some((mm, ss)) = ts.split_once(':') {
                if let (Ok(m), Ok(sec)) = (mm.parse::<u64>(), ss.parse::<f64>()) {
                    timestamps.push((m as f64) * 60.0 + sec);
                }
            }
            cursor = &cursor[end_br + 1..];
        }
        let text = cursor.trim();
        if text.is_empty() || timestamps.is_empty() {
            continue;
        }
        for t in timestamps {
            raw_lines.push((t, text.to_string()));
        }
    }
    raw_lines.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut out = Vec::with_capacity(raw_lines.len());
    for i in 0..raw_lines.len() {
        let (t_start, ref text) = raw_lines[i];
        let t_end = if i + 1 < raw_lines.len() {
            raw_lines[i + 1].0
        } else {
            t_start + 3.5
        };
        let words = build_word_timings(text, t_start, t_end, env);
        let clean_text = if text.contains('<') && text.contains('>') {
            words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            text.clone()
        };
        out.push(LrcLine {
            time_sec: t_start,
            text: clean_text,
            words,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_and_midline_adlibs_overlap_main_vocals_on_same_line() {
        let trailing = build_word_timings(
            "Go up, go up, go up, go down (fuck that, pop out)",
            33.33,
            36.69,
            None,
        );
        let last_main = trailing.iter().filter(|w| !w.is_adlib).last().unwrap();
        let first_adlib = trailing.iter().find(|w| w.is_adlib).unwrap();
        // Adlib starts BEFORE the main vocal finishes so both play at the same time on the same line!
        assert!(
            first_adlib.start < last_main.end,
            "trailing adlib should overlap main vocal: adlib_start={} main_end={}",
            first_adlib.start,
            last_main.end
        );

        let midline = build_word_timings(
            "I can't choose (can't choose), I can't decide with these bitches",
            45.87,
            48.96,
            None,
        );
        let adlib_words: Vec<&LrcWord> = midline.iter().filter(|w| w.is_adlib).collect();
        let after_choose = midline.iter().find(|w| w.text == "I" && w.start > 46.0).unwrap();
        assert!(!adlib_words.is_empty());
        assert!(
            adlib_words[0].start < after_choose.end && adlib_words.last().unwrap().end > after_choose.start,
            "mid-line adlib should play simultaneously with following main words"
        );
    }

    #[test]
    fn short_line_does_not_crawl_across_long_beat_gap() {
        let words = build_word_timings("Pull up, who that is? (Errt, errt)", 41.22, 44.73, None);
        let main_end = words.iter().filter(|w| !w.is_adlib).last().unwrap().end;
        // Main vocal (5 syllables) should finish naturally well before the 3.51s beat gap ends
        assert!(
            main_end - 41.22 < 2.6,
            "short line stretched too slow: dur={:.2}s",
            main_end - 41.22
        );
    }
}

