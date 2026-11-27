//! Explicit-word filter: masks text on screen and works out which parts of a song
//! to muffle from the word timings.

use crate::{cache::home_dir, lyrics::LrcLine, names::fold};
use ratatui::{buffer::Buffer, layout::Rect};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs};

/// How a censored word sounds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Style {
    /// The centre of the mix (the lead voice) ducks; bass and stereo beat stay.
    #[default]
    VocalDip,
    /// The whole mix goes dull for a moment.
    Underwater,
}

impl Style {
    pub fn next(self) -> Self {
        match self {
            Style::VocalDip => Style::Underwater,
            Style::Underwater => Style::VocalDip,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Style::VocalDip => "Vocal dip",
            Style::Underwater => "Underwater",
        }
    }

    /// The ffmpeg filters that make up the censored sound.
    fn effect(self) -> &'static str {
        match self {
            // Split into mid/side, take the voice band out of each, join again.
            Style::VocalDip => concat!(
                "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0-0.5*c1,",
                "lowpass=f=550:c=FL,lowpass=f=550:c=FL,",
                "equalizer=f=1300:t=o:w=2.6:g=-12:c=FR,",
                "pan=stereo|c0=c0+c1|c1=c0-c1"
            ),
            Style::Underwater => "lowpass=f=550,lowpass=f=550",
        }
    }
}

/// Seconds spent swerving into and out of the censored sound.
const FADE_IN: f64 = 0.10;
const FADE_OUT: f64 = 0.12;

/// `a+b+c...` bracketed as a balanced tree; ffmpeg limits how deeply sums may nest.
fn sum(terms: &[String]) -> String {
    match terms {
        [one] => one.clone(),
        _ => {
            let (a, b) = terms.split_at(terms.len() / 2);
            format!("({}+{})", sum(a), sum(b))
        }
    }
}

/// An ffmpeg graph that crossfades from the song into `style` around each range and
/// back again. Ranges must be further apart than the two fades. `None` if empty.
pub fn filter_graph(ranges: &[(f64, f64)], style: Style) -> Option<String> {
    if ranges.is_empty() {
        return None;
    }
    // 0 = untouched, 1 = fully censored, sloping between the two.
    let ramps: Vec<String> = ranges
        .iter()
        .map(|(a, b)| {
            format!(
                "clip(min((t-{:.3})/{FADE_IN},({:.3}-t)/{FADE_OUT}),0,1)",
                a - FADE_IN,
                b + FADE_OUT
            )
        })
        .collect();
    let amount = sum(&ramps);
    let effect = style.effect();
    Some(format!(
        "aformat=channel_layouts=stereo,asetnsamples=n=256:p=0,asplit[dry][wet];\
         [wet]{effect},volume=volume='{amount}':eval=frame[censored];\
         [dry]volume=volume='1-{amount}':eval=frame[clear];\
         [clear][censored]amix=inputs=2:normalize=0"
    ))
}

/// Matched anywhere inside a word.
const ROOTS: &[&str] = &[
    "fuck", "shit", "bitch", "biatch", "nigga", "nigger", "niggu", "pussy", "pussies", "asshole",
    "cunt", "whore", "faggot", "cocksuck", "dickhead", "muhfuk", "mfer",
];

/// Matched only as a whole word, so "class", "Dickens" or "cumin" are left alone.
const WORDS: &[&str] = &[
    "ass", "asses", "azz", "dumbass", "badass", "fatass", "bigass", "jackass", "lazyass",
    "dick", "dicks", "cock", "cocks", "cum", "cums", "cumming", "cummin", "hoe", "hoes", "hos",
    "thot", "thots", "slut", "sluts", "slutty", "bastard", "bastards", "piss", "pissed",
    "pissin", "pissing", "tit", "tits", "titty", "titties", "porn", "porno", "fag", "fags",
    "retard", "retards", "retarded", "dildo", "blowjob", "horny", "nig", "nigs", "mf", "wtf",
    "stfu", "fk", "fkn",
];

/// Collapses runs of the same letter longer than `keep` ("fuuuck" -> "fuck").
fn squeeze(word: &str, keep: usize) -> String {
    let chars: Vec<char> = word.chars().collect();
    chars
        .chunk_by(|a, b| a == b)
        .flat_map(|run| {
            let n = if run.len() >= 3 { keep } else { run.len() };
            run[..n].iter()
        })
        .collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\'' | '’' | '*' | '$')
}

#[derive(Default)]
pub struct Censor {
    extra: HashSet<String>,
}

impl Censor {
    /// Built-in list plus one word per line from `~/.config/itmp/censor.txt`.
    pub fn load() -> Self {
        let extra = fs::read_to_string(home_dir().join(".config/itmp/censor.txt"))
            .unwrap_or_default()
            .lines()
            .map(Self::plain)
            .filter(|w| !w.is_empty() && !w.starts_with('#'))
            .collect();
        Censor { extra }
    }

    fn plain(word: &str) -> String {
        word.trim()
            .chars()
            .filter(|c| !matches!(c, '\'' | '’'))
            .map(|c| if c == '$' { 's' } else { fold(c) })
            .collect()
    }

    /// `part` is a single word without spaces or hyphens.
    fn bad_part(&self, part: &str) -> bool {
        // Already starred out by whoever wrote the lyrics ("f**k").
        if part.contains('*') {
            return part.chars().next().is_some_and(char::is_alphabetic);
        }
        let word = Self::plain(part);
        [word.clone(), squeeze(&word, 1), squeeze(&word, 2)]
            .iter()
            .any(|w| {
                ROOTS.iter().any(|r| w.contains(r))
                    || WORDS.contains(&w.as_str())
                    || self.extra.contains(w)
            })
    }

    pub fn is_bad(&self, token: &str) -> bool {
        token
            .split(|c| !is_word_char(c))
            .any(|p| !p.is_empty() && self.bad_part(p))
    }

    /// Stars out bad words on the drawn screen, keeping their first letter.
    pub fn scrub(&self, buf: &mut Buffer, skip: &[Rect]) {
        let area = buf.area;
        for y in area.top()..area.bottom() {
            let mut word = String::new();
            let mut cells: Vec<u16> = Vec::new();
            for x in area.left()..=area.right() {
                let letter = (x < area.right()
                    && !skip.iter().any(|r| r.contains((x, y).into())))
                .then(|| {
                    let mut chars = buf[(x, y)].symbol().chars();
                    // Image cells hold long escape sequences; leave those alone.
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if is_word_char(c) => Some(c),
                        _ => None,
                    }
                })
                .flatten();
                match letter {
                    Some(c) => {
                        word.push(c);
                        cells.push(x);
                    }
                    None => {
                        if !word.is_empty() && self.bad_part(&word) {
                            for &cx in cells.iter().skip(1) {
                                buf[(cx, y)].set_symbol("*");
                            }
                        }
                        word.clear();
                        cells.clear();
                    }
                }
            }
        }
    }

    /// Where (in seconds of audio) the censored sound is at full strength. Kept
    /// deliberately short: a word slipping through a little beats muffling the
    /// words around it. `offset` is the user's lyric nudge.
    pub fn audio_ranges(&self, lines: &[LrcLine], offset: f64) -> Vec<(f64, f64)> {
        let mut ranges = Vec::new();
        for line in lines {
            // Estimated word times are less trustworthy, so cover even less.
            let longest = if line.aligned { 0.45 } else { 0.35 };
            ranges.extend(line.words.iter().filter(|w| self.is_bad(&w.text)).map(|w| {
                // The aligner often clips word ends, hence the floor.
                let end = w.end.min(w.start + longest).max(w.start + 0.24);
                (
                    (w.start + 0.02 - offset).max(0.0),
                    end - 0.02 - offset,
                )
            }));
        }
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (start, end) in ranges {
            match merged.last_mut() {
                // No room to come out and go back in: stay under.
                Some(last) if start - last.1 <= FADE_IN + FADE_OUT => last.1 = last.1.max(end),
                _ if end > start => merged.push((start, end)),
                _ => {}
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_explicit_words_without_false_positives() {
        let c = Censor::default();
        for bad in ["fuck,", "(Bitches)", "bitch-ass", "sh**", "fuuuck", "a$$", "niggas'", "Hoes"] {
            assert!(c.is_bad(bad), "{bad}");
        }
        for fine in ["class", "pass", "Dickens", "cumin", "shoes", "assist", "Talk", "*laughs*", "hello"] {
            assert!(!c.is_bad(fine), "{fine}");
        }
    }

    #[test]
    fn covers_only_the_bad_word_when_timing_is_measured() {
        let word = |text: &str, start: f64| crate::lyrics::LrcWord {
            text: text.into(),
            start,
            end: start + 0.4,
            is_adlib: false,
        };
        let mut line = LrcLine {
            time_sec: 10.0,
            text: String::new(),
            words: vec![word("oh", 10.0), word("shit", 11.0), word("yeah", 12.0)],
            aligned: true,
        };
        let c = Censor::default();
        let measured = c.audio_ranges(std::slice::from_ref(&line), 0.0);
        assert_eq!(measured.len(), 1);
        assert!(measured[0].0 >= 11.0 && measured[0].1 <= 11.4);
        // Even with estimated timing only the word itself is touched, never the line.
        line.aligned = false;
        let rough = c.audio_ranges(std::slice::from_ref(&line), 0.0);
        assert!(rough[0].0 >= 11.0 && rough[0].1 <= 11.35);
        // A long-held word is only covered at its start.
        line.words[1].end = 14.0;
        assert!(c.audio_ranges(&[line], 0.0)[0].1 < 11.4);
    }
}
