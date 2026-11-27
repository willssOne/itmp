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
    /// True when the word times were measured from the audio, not estimated.
    pub aligned: bool,
}

/// Compact 15ms (~66.7 Hz) Mid-Side vocal-isolated energy & onset envelope (380-2800 Hz).
/// Lead vocals sit dead-center (Mid = (L+R)/2) while stereo synths/pads/reverb sit in Side = (L-R)/2.
/// Subtracting Side from Mid strips dense stereo beats cleanly and isolates the actual voice.
#[derive(Debug, Clone)]
pub struct AudioEnvelope {
    pub frame_sec: f64,
    pub vox: Vec<f32>,
    pub onset: Vec<f32>,
}

impl AudioEnvelope {
    pub fn extract(audio_path: &Path) -> Option<Self> {
        // Output stereo: ch0 = Mid (L+R)/2, ch1 = Side (L-R)/2, vocal-band filtered 380-2800 Hz
        let out = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                audio_path.to_str()?,
                "-ar",
                "8000",
                "-af",
                "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0-0.5*c1,highpass=f=380,lowpass=f=2800",
                "-f",
                "s16le",
                "-",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !out.status.success() || out.stdout.len() < 960 {
            return None;
        }

        // 120 stereo samples at 8000 Hz = 15ms per frame (4 bytes per stereo sample)
        const SAMPLES_PER_FRAME: usize = 120;
        const BYTES_PER_FRAME: usize = SAMPLES_PER_FRAME * 4;
        let frame_count = out.stdout.len() / BYTES_PER_FRAME;
        if frame_count < 12 {
            return None;
        }

        let mut mid_rms = Vec::with_capacity(frame_count);
        let mut side_rms = Vec::with_capacity(frame_count);
        let mut total_side = 0.0f64;
        let mut total_mid = 0.0f64;

        for f in 0..frame_count {
            let base = f * BYTES_PER_FRAME;
            let mut m_sq = 0.0f64;
            let mut s_sq = 0.0f64;
            for s in 0..SAMPLES_PER_FRAME {
                let idx = base + s * 4;
                let m = i16::from_le_bytes([out.stdout[idx], out.stdout[idx + 1]]) as f64 / 32768.0;
                let si =
                    i16::from_le_bytes([out.stdout[idx + 2], out.stdout[idx + 3]]) as f64 / 32768.0;
                m_sq += m * m;
                s_sq += si * si;
            }
            let mr = (m_sq / SAMPLES_PER_FRAME as f64).sqrt() as f32;
            let sr = (s_sq / SAMPLES_PER_FRAME as f64).sqrt() as f32;
            total_mid += mr as f64;
            total_side += sr as f64;
            mid_rms.push(mr);
            side_rms.push(sr);
        }

        // If track is stereo, subtract 1.05 * side_rms from mid_rms to strip stereo synths/pads!
        // If track is purely mono (side ~ 0), gracefully fall back to mid_rms.
        let use_mid_side = total_mid > 0.0 && (total_side / total_mid) > 0.12;
        let mut raw_vox = Vec::with_capacity(frame_count);
        for i in 0..frame_count {
            let v = if use_mid_side {
                (mid_rms[i] - 1.05 * side_rms[i]).max(0.0)
            } else {
                mid_rms[i]
            };
            raw_vox.push(v);
        }

        // 5-frame smoothing (~75ms) rejects brief snare clicks while preserving vocal syllables
        let mut vox = Vec::with_capacity(frame_count);
        for i in 0..frame_count {
            let a = raw_vox[i.saturating_sub(2)];
            let b = raw_vox[i.saturating_sub(1)];
            let c = raw_vox[i];
            let d = raw_vox[(i + 1).min(frame_count - 1)];
            let e = raw_vox[(i + 2).min(frame_count - 1)];
            vox.push(0.10 * a + 0.20 * b + 0.40 * c + 0.20 * d + 0.10 * e);
        }

        let mut onset = Vec::with_capacity(frame_count);
        onset.push(0.0);
        for i in 1..frame_count {
            onset.push((vox[i] - vox[i - 1]).max(0.0));
        }

        Some(Self {
            frame_sec: 0.015,
            vox,
            onset,
        })
    }

    fn time_to_idx(&self, t: f64) -> usize {
        if self.vox.is_empty() {
            return 0;
        }
        ((t.max(0.0) / self.frame_sec).floor() as usize).min(self.vox.len() - 1)
    }

    fn local_floor_and_peak(&self, i0: usize, i1: usize) -> (f32, f32) {
        let slice = &self.vox[i0..=i1];
        let mut sorted: Vec<f32> = slice.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let floor = sorted[sorted.len() / 5];
        let peak = sorted[(sorted.len() * 92) / 100].max(floor + 1e-4);
        (floor, peak)
    }

    /// Detects the true vocal start, main vocal end, and trailing adlib end inside `[t_start, t_end]`.
    /// Because Mid-Side subtraction drops pure instrumental beats to ~0, we can accurately snap
    /// late vocal entries (up to +1.15s) and strip trailing instrumental gaps without beat interference!
    fn detect_vocal_bounds(
        &self,
        t_start: f64,
        t_end: f64,
        nat_min: f64,
        nat_max: f64,
    ) -> (f64, f64, f64) {
        let i0 = self.time_to_idx(t_start);
        let i1 = self.time_to_idx((t_end - 0.03).max(t_start + 0.25));
        if i1 <= i0 + 5 {
            let ve = (t_start + nat_max).min(t_end - 0.04).max(t_start + 0.25);
            return (t_start, ve, ve);
        }

        let (floor, peak) = self.local_floor_and_peak(i0, i1);
        let range = (peak - floor).max(1e-4);
        let slice = &self.vox[i0..=i1];

        // 1. Lead-in trim: only advance if the voice is genuinely silent at t_start!
        //    Allows snapping up to +1.15s (capped at 42% of bar) when an LRC line precedes an instrumental drop.
        let lead_thresh = floor + 0.22 * range;
        let max_lead_sec = ((t_end - t_start) * 0.42).min(1.15);
        let max_lead_frames = ((max_lead_sec / self.frame_sec) as usize).min(slice.len() / 2);
        let mut lead_off = 0usize;
        if slice[0] < lead_thresh {
            for (k, &v) in slice.iter().take(max_lead_frames).enumerate() {
                if v >= lead_thresh {
                    lead_off = k.saturating_sub(2); // include 30ms consonant attack
                    break;
                }
            }
        }
        let v_start = t_start + (lead_off as f64) * self.frame_sec;

        // 2. Full vocal tail (captures trailing adlibs)
        let tail_thresh = floor + 0.16 * range;
        let mut last_active_full = slice.len() - 1;
        for k in (lead_off..slice.len()).rev() {
            if slice[k] >= tail_thresh {
                last_active_full = k;
                break;
            }
        }
        let vocal_tail_end = (t_start + (last_active_full as f64 + 1.0) * self.frame_sec)
            .min(t_end - 0.03)
            .max(v_start + 0.25);

        // 3. Main vocal end: natural vocal pace bounded by actual Mid-Side vocal drop-off
        let cap_end = (v_start + nat_max).min(vocal_tail_end);
        let cap_rel = ((cap_end - t_start) / self.frame_sec)
            .floor()
            .clamp(lead_off as f64, (slice.len() - 1) as f64) as usize;
        let main_thresh = floor + 0.20 * range;
        let mut last_main = cap_rel;
        for k in (lead_off..=cap_rel).rev() {
            if slice[k] >= main_thresh {
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

    fn build_cdf(&self, start_t: f64, end_t: f64) -> Option<(usize, Vec<f64>)> {
        let i0 = self.time_to_idx(start_t);
        let i1 = self.time_to_idx(end_t);
        if i1 <= i0 + 3 {
            return None;
        }
        let n = i1 - i0 + 1;
        let (floor, peak) = self.local_floor_and_peak(i0, i1);
        let range = (peak - floor).max(1e-4);

        let mut max_ons = 1e-4f32;
        for i in i0..=i1 {
            max_ons = max_ons.max(self.onset[i]);
        }

        let mut cdf = Vec::with_capacity(n + 1);
        cdf.push(0.0);
        let mut acc = 0.0f64;
        for i in i0..=i1 {
            let excess = ((self.vox[i] - floor) / range).clamp(0.0, 1.0) as f64;
            let no = (self.onset[i] / max_ons) as f64;
            let act = 0.12 + 0.64 * excess.powf(1.25) + 0.24 * no;
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

    fn warp_time(
        &self,
        start_t: f64,
        end_t: f64,
        cdf_opt: Option<&(usize, Vec<f64>)>,
        u: f64,
    ) -> f64 {
        let u = u.clamp(0.0, 1.0);
        let lin = start_t + u * (end_t - start_t);
        let Some((i0, cdf)) = cdf_opt else {
            return lin;
        };
        let n = cdf.len().saturating_sub(1);
        if n == 0 {
            return lin;
        }
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
        0.35 * lin + 0.65 * audio_clamped
    }
}

#[derive(Debug, Clone)]
enum LineToken {
    Main(String),
    Adlib { text: String, group_id: usize },
}

struct AdlibGroup {
    anchor_main_idx: Option<usize>,
    is_trailing: bool,
    word_indices: Vec<usize>,
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

fn split_main_and_adlibs(line_text: &str) -> Vec<LineToken> {
    let raw: Vec<&str> = line_text.split_whitespace().collect();
    let mut tokens = Vec::with_capacity(raw.len());
    let mut in_paren = false;
    let mut group_id = 0usize;
    let mut has_main = false;

    for w in &raw {
        let starts_p = w.starts_with('(');
        let ends_p = w.ends_with(')')
            || w.ends_with("),")
            || w.ends_with(").")
            || w.ends_with(")?")
            || w.ends_with(")!");
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
        0.06
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
        we = we.max(ws + 0.07).min(start_t + span);
        if we <= ws {
            we = ws + 0.07;
        }
        out.push((ws, we));
        prev_end = we;
    }
    out
}

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
    let mut main_word_slots: Vec<usize> = Vec::new();
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

    let main_syls: usize = main_texts.iter().map(|w| count_syllables(w)).sum();
    let main_pauses: f64 = main_texts
        .iter()
        .enumerate()
        .map(|(i, w)| word_weight_and_pause(w, i + 1 == total_main).1)
        .sum();
    let span = (t_end - t_start).clamp(0.30, 8.5);
    let nat_min = ((main_syls as f64) * 0.11 + main_pauses * 0.12)
        .clamp(0.28, (span * 0.85).max(0.28));
    let nat_max = ((main_syls as f64) * 0.30 + main_pauses * 0.25 + 0.22)
        .clamp(nat_min + 0.15, (span * 0.94).max(nat_min + 0.15));

    let (v_start, v_end, vocal_tail_end) = match env {
        Some(e) => e.detect_vocal_bounds(t_start, t_start + span, nat_min, nat_max),
        None => {
            let vs = t_start + (span * 0.05).min(0.10);
            let ve = (vs + nat_max.min(span * 0.86)).max(vs + nat_min);
            (vs, ve, (t_start + span * 0.92).max(ve))
        }
    };

    let main_timings = assign_sequence_timings(&main_texts, v_start, v_end, env);
    for (&slot, &(ws, we)) in main_word_slots.iter().zip(main_timings.iter()) {
        out[slot].start = ws;
        out[slot].end = we;
    }

    for g in &groups {
        let adlib_texts: Vec<String> = g
            .word_indices
            .iter()
            .map(|&idx| out[idx].text.clone())
            .collect();
        let adlib_syls: usize = adlib_texts.iter().map(|w| count_syllables(w)).sum();
        let adlib_pauses: f64 = adlib_texts
            .iter()
            .enumerate()
            .map(|(i, w)| word_weight_and_pause(w, i + 1 == adlib_texts.len()).1)
            .sum();
        let nat_adlib_dur = ((adlib_syls as f64) * 0.20 + adlib_pauses * 0.16 + 0.14).clamp(0.30, 2.2);

        let (a_start, a_end) = if g.is_trailing {
            let overlap_words = if adlib_texts.len() >= 3 { 3 } else { 2 };
            let overlap_idx = total_main.saturating_sub(overlap_words);
            let overlap_t = main_timings
                .get(overlap_idx)
                .map(|&(s, _)| s)
                .unwrap_or(v_start);
            let min_start = (v_start + (v_end - v_start) * 0.45).min(v_end - 0.15);
            let a_s = overlap_t.max(min_start).max(v_start);
            let target_end =
                (a_s + nat_adlib_dur).max(vocal_tail_end.min(a_s + nat_adlib_dur * 1.30));
            let a_e = target_end.min(t_start + span - 0.03).max(a_s + 0.26);
            (a_s, a_e)
        } else if let Some(anchor) = g.anchor_main_idx {
            let anchor_end = main_timings
                .get(anchor)
                .map(|&(_, e)| e)
                .unwrap_or(v_start);
            let a_s = (anchor_end - 0.06).max(v_start);
            let a_e = (a_s + nat_adlib_dur)
                .min(t_start + span - 0.03)
                .max(a_s + 0.26);
            (a_s, a_e)
        } else {
            let a_s = v_start;
            let a_e = (a_s + nat_adlib_dur).min(v_end).max(a_s + 0.26);
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
    // Keep empty-text timestamp markers (e.g. `[00:09.94] `) as end-boundaries for the previous line!
    let mut raw_events: Vec<(f64, String)> = Vec::new();
    for line in raw.lines() {
        let s = line.trim();
        if !s.starts_with('[') {
            continue;
        }
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
        if timestamps.is_empty() {
            continue;
        }
        let text = cursor.trim().to_string();
        for t in timestamps {
            raw_events.push((t, text.clone()));
        }
    }
    raw_events.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut out = Vec::with_capacity(raw_events.len());
    for i in 0..raw_events.len() {
        let (t_start, ref text) = raw_events[i];
        if text.is_empty() {
            continue;
        }
        let t_end = if i + 1 < raw_events.len() {
            raw_events[i + 1].0
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
            aligned: false,
        });
    }
    apply_alignment(&mut out, &crate::sync::words_path(dir, filename));
    out
}

/// Replaces estimated word times with the measured ones from `<stem>.words.json`.
fn apply_alignment(lines: &mut [LrcLine], path: &Path) {
    let Some(doc) = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return;
    };
    let Some(measured) = doc["lines"].as_array().filter(|a| !a.is_empty()) else {
        return;
    };
    let offset = doc["offset"].as_f64().unwrap_or(0.0);
    let mut previous = 0.0f64;
    for line in lines.iter_mut() {
        let stamp = line.time_sec;
        let hit = measured
            .iter()
            .find(|m| (m["t"].as_f64().unwrap_or(-9.0) - stamp).abs() < 0.005)
            .and_then(|m| m["words"].as_array())
            .filter(|w| w.len() == line.words.len());
        if let Some(times) = hit {
            for (word, t) in line.words.iter_mut().zip(times) {
                word.start = t[0].as_f64().unwrap_or(word.start);
                word.end = t[1].as_f64().unwrap_or(word.end);
            }
            line.aligned = true;
            // Show the line a beat before its first word so the eye can get there.
            line.time_sec = line.words.first().map(|w| w.start - 0.15).unwrap_or(stamp);
        } else {
            for word in &mut line.words {
                word.start += offset;
                word.end += offset;
            }
            line.time_sec = stamp + offset;
        }
        line.time_sec = line.time_sec.max(previous + 0.01).max(0.0);
        previous = line.time_sec;
    }
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
            adlib_words[0].start < after_choose.end
                && adlib_words.last().unwrap().end > after_choose.start,
            "mid-line adlib should play simultaneously with following main words"
        );
    }

    #[test]
    fn short_line_does_not_crawl_across_long_beat_gap() {
        let words = build_word_timings("Pull up, who that is? (Errt, errt)", 41.22, 44.73, None);
        let main_end = words.iter().filter(|w| !w.is_adlib).last().unwrap().end;
        assert!(
            main_end - 41.22 < 2.5,
            "short line stretched too slow: dur={:.2}s",
            main_end - 41.22
        );
    }
}
