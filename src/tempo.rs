//! Playback speed and pitch. While linked (the default) they move together like a
//! record player: changing either one sets the other to match.

use serde::{Deserialize, Serialize};

const MIN_SPEED: f64 = 0.5;
const MAX_SPEED: f64 = 2.0;
/// One octave, which is exactly what the speed limits allow when linked.
const MAX_SEMITONES: f64 = 12.0;
const SPEED_STEP: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tune {
    pub speed: f64,
    /// Pitch shift; 0 is the original key.
    pub semitones: f64,
    pub linked: bool,
}

impl Default for Tune {
    fn default() -> Self {
        Tune {
            speed: 1.0,
            semitones: 0.0,
            linked: true,
        }
    }
}

/// The next multiple of `size` above or below `value`.
fn step(value: f64, size: f64, up: bool) -> f64 {
    let at = value / size;
    // The slack keeps a value already on the grid from rounding back onto itself.
    let next = if up {
        (at + 1e-6).floor() + 1.0
    } else {
        (at - 1e-6).ceil() - 1.0
    };
    next * size
}

impl Tune {
    pub fn step_speed(&mut self, up: bool) {
        let next = step(self.speed, SPEED_STEP, up);
        self.speed = ((next * 100.0).round() / 100.0).clamp(MIN_SPEED, MAX_SPEED);
        if self.linked {
            self.semitones = 12.0 * self.speed.log2();
        }
    }

    pub fn step_pitch(&mut self, up: bool) {
        self.semitones = step(self.semitones, 1.0, up).clamp(-MAX_SEMITONES, MAX_SEMITONES);
        if self.linked {
            self.speed = self.pitch_factor();
        }
    }

    /// Linking again keeps the speed and moves the pitch to match it.
    pub fn toggle_link(&mut self) {
        self.linked = !self.linked;
        if self.linked {
            self.semitones = 12.0 * self.speed.log2();
        }
    }

    /// Back to the original sound; the link setting stays.
    pub fn reset(&mut self) {
        *self = Tune {
            linked: self.linked,
            ..Tune::default()
        };
    }

    /// Frequency multiplier of the pitch shift.
    pub fn pitch_factor(&self) -> f64 {
        (self.semitones / 12.0).exp2()
    }

    /// Repairs values from a hand-edited or damaged library file.
    pub fn sanitized(mut self) -> Self {
        let sane = |v: f64, fallback: f64| if v.is_finite() { v } else { fallback };
        self.speed = sane(self.speed, 1.0).clamp(MIN_SPEED, MAX_SPEED);
        self.semitones = sane(self.semitones, 0.0).clamp(-MAX_SEMITONES, MAX_SEMITONES);
        if self.linked {
            self.semitones = 12.0 * self.speed.log2();
        }
        self
    }

    pub fn speed_label(&self) -> String {
        format!("{:.2}x", self.speed)
    }

    pub fn pitch_label(&self) -> String {
        let st = (self.semitones * 10.0).round() / 10.0;
        if st == 0.0 {
            "0 st".into()
        } else if st.fract() == 0.0 {
            format!("{st:+.0} st")
        } else {
            format!("{st:+.1} st")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn linked_speed_and_pitch_follow_each_other() {
        let mut t = Tune::default();
        t.step_speed(true);
        assert!(near(t.speed, 1.05) && near(t.semitones, 12.0 * 1.05f64.log2()));
        // From between two semitones, a step lands on the next whole one.
        t.step_pitch(true);
        assert!(near(t.semitones, 1.0) && near(t.speed, (1.0f64 / 12.0).exp2()));
        t.step_speed(false);
        assert!(near(t.speed, 1.05));
        for _ in 0..40 {
            t.step_pitch(false);
        }
        assert!(near(t.semitones, -12.0) && near(t.speed, 0.5));
        for _ in 0..40 {
            t.step_speed(true);
        }
        assert!(near(t.speed, 2.0) && near(t.semitones, 12.0));
    }

    #[test]
    fn unlinked_values_are_independent_until_relinked() {
        let mut t = Tune::default();
        t.toggle_link();
        t.step_speed(false);
        assert!(near(t.speed, 0.95) && near(t.semitones, 0.0));
        t.step_pitch(true);
        assert!(near(t.speed, 0.95) && near(t.semitones, 1.0));
        t.toggle_link();
        assert!(near(t.speed, 0.95) && near(t.semitones, 12.0 * 0.95f64.log2()));
        t.reset();
        assert_eq!(t, Tune::default());
    }

    #[test]
    fn labels_and_damaged_values() {
        let mut t = Tune::default();
        assert_eq!((t.speed_label(), t.pitch_label()), ("1.00x".into(), "0 st".into()));
        t.step_pitch(false);
        assert_eq!(t.pitch_label(), "-1 st");
        let bad = Tune {
            speed: f64::NAN,
            semitones: 99.0,
            linked: false,
        }
        .sanitized();
        assert!(near(bad.speed, 1.0) && near(bad.semitones, 12.0));
    }
}
