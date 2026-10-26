//! The scripted participant's synthetic media: speech-like audio bursts on a talk script, and
//! camera frames of a solid colour with a moving bar. Deterministic, so a test can reason about
//! what the engine received.

/// An RGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// A named colour (`red`, `green`, `blue`, `orange`, `purple`, `teal`, `grey`) or
    /// `#rrggbb` / `rrggbb`.
    pub fn parse(raw: &str) -> Result<Rgb, String> {
        let named = match raw.trim().to_ascii_lowercase().as_str() {
            "red" => Some(Rgb(200, 40, 40)),
            "green" => Some(Rgb(40, 170, 60)),
            "blue" => Some(Rgb(40, 70, 200)),
            "orange" => Some(Rgb(230, 140, 30)),
            "purple" => Some(Rgb(130, 50, 170)),
            "teal" => Some(Rgb(30, 150, 150)),
            "grey" | "gray" => Some(Rgb(128, 128, 128)),
            _ => None,
        };
        if let Some(colour) = named {
            return Ok(colour);
        }
        let hex = raw.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("'{raw}' is not a colour name or #rrggbb"));
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
        Ok(Rgb(byte(0), byte(2), byte(4)))
    }

    /// BT.601 video-range Y, Cb, Cr.
    pub fn to_ycbcr(self) -> (u8, u8, u8) {
        let (r, g, b) = (
            f32::from(self.0) / 255.0,
            f32::from(self.1) / 255.0,
            f32::from(self.2) / 255.0,
        );
        let y = 16.0 + 65.481 * r + 128.553 * g + 24.966 * b;
        let cb = 128.0 - 37.797 * r - 74.203 * g + 112.0 * b;
        let cr = 128.0 + 112.0 * r - 93.786 * g - 18.214 * b;
        (
            y.round().clamp(0.0, 255.0) as u8,
            cb.round().clamp(0.0, 255.0) as u8,
            cr.round().clamp(0.0, 255.0) as u8,
        )
    }
}

/// Fill an I420 frame with `colour` and a white vertical bar a tenth of the frame wide whose
/// left edge sits at `bar` (0.0 = left, 1.0 = right). Planes are `(data, stride)`.
pub fn fill_i420(
    width: u32,
    height: u32,
    y: (&mut [u8], u32),
    u: (&mut [u8], u32),
    v: (&mut [u8], u32),
    colour: Rgb,
    bar: f32,
) {
    let (cy, cu, cv) = colour.to_ycbcr();
    let (wy, wu, wv) = Rgb(235, 235, 235).to_ycbcr();
    let bar_width = (width / 10).max(2);
    let bar_left = ((width.saturating_sub(bar_width)) as f32 * bar.clamp(0.0, 1.0)) as u32;
    let in_bar = |x: u32| x >= bar_left && x < bar_left + bar_width;
    let (y_plane, y_stride) = y;
    for row in 0..height {
        for x in 0..width {
            y_plane[(row * y_stride + x) as usize] = if in_bar(x) { wy } else { cy };
        }
    }
    let (chroma_w, chroma_h) = (width.div_ceil(2), height.div_ceil(2));
    let (u_plane, u_stride) = u;
    let (v_plane, v_stride) = v;
    for row in 0..chroma_h {
        for x in 0..chroma_w {
            let bar_here = in_bar(x * 2);
            u_plane[(row * u_stride + x) as usize] = if bar_here { wu } else { cu };
            v_plane[(row * v_stride + x) as usize] = if bar_here { wv } else { cv };
        }
    }
}

/// A talk script: the stretches, in seconds from the participant's join, in which it speaks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TalkScript {
    pub stretches: Vec<(f64, f64)>,
}

impl TalkScript {
    /// Parse `"1.0-3.0,6.5-8.5"`. Empty = never talks.
    pub fn parse(raw: &str) -> Result<TalkScript, String> {
        let mut stretches = Vec::new();
        for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (start, end) = part
                .split_once('-')
                .ok_or_else(|| format!("talk stretch '{part}' is not <start>-<end>"))?;
            let parse = |s: &str| {
                s.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .ok_or_else(|| format!("talk stretch '{part}': '{s}' is not a time in seconds"))
            };
            let (start, end) = (parse(start)?, parse(end)?);
            if end <= start {
                return Err(format!("talk stretch '{part}' ends before it starts"));
            }
            stretches.push((start, end));
        }
        stretches.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(TalkScript { stretches })
    }

    pub fn is_talking(&self, at_secs: f64) -> bool {
        self.stretches
            .iter()
            .any(|(start, end)| at_secs >= *start && at_secs < *end)
    }

    /// Total talk time in seconds.
    pub fn total_secs(&self) -> f64 {
        self.stretches.iter().map(|(s, e)| e - s).sum()
    }
}

/// One sample of speech-like sound at `at_secs`: a fundamental at `tone_hz` with two
/// harmonics, a slight vibrato, and a 4 Hz syllable envelope. Peak about 12000 (RMS several
/// thousand — far above any energy-VAD threshold).
pub fn speech_sample(at_secs: f64, tone_hz: f64) -> i16 {
    use std::f64::consts::PI;
    let vibrato = 1.0 + 0.03 * (2.0 * PI * 5.0 * at_secs).sin();
    let phase = 2.0 * PI * tone_hz * vibrato * at_secs;
    let voice = phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin();
    let envelope = 0.55 + 0.45 * (2.0 * PI * 4.0 * at_secs).sin();
    (voice / 1.75 * envelope * 12_000.0) as i16
}

/// `count` samples at `rate` starting at `start_secs` of the script: speech inside a talk
/// stretch, digital silence outside.
pub fn script_samples(
    script: &TalkScript,
    tone_hz: f64,
    rate: u32,
    start_secs: f64,
    count: usize,
) -> Vec<i16> {
    (0..count)
        .map(|i| {
            let t = start_secs + i as f64 / f64::from(rate);
            if script.is_talking(t) {
                speech_sample(t, tone_hz)
            } else {
                0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talk_scripts_parse_and_answer() {
        let script = TalkScript::parse("6.5-8.5, 1.0-3.0").unwrap();
        assert_eq!(script.stretches, vec![(1.0, 3.0), (6.5, 8.5)]);
        assert!(!script.is_talking(0.5));
        assert!(script.is_talking(1.0));
        assert!(script.is_talking(7.0));
        assert!(!script.is_talking(8.5));
        assert!((script.total_secs() - 4.0).abs() < 1e-9);
        assert_eq!(TalkScript::parse("").unwrap().stretches, vec![]);
        assert!(TalkScript::parse("3-1").is_err());
        assert!(TalkScript::parse("1-x").is_err());
        assert!(TalkScript::parse("-1-2").is_err());
        assert!(TalkScript::parse("12").is_err());
    }

    #[test]
    fn speech_is_loud_inside_a_stretch_and_silent_outside() {
        let script = TalkScript::parse("1.0-2.0").unwrap();
        let rms = |s: &[i16]| {
            (s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
        };
        let talking = script_samples(&script, 180.0, 48_000, 1.2, 9600);
        assert!(rms(&talking) > 2000.0, "rms {}", rms(&talking));
        let quiet = script_samples(&script, 180.0, 48_000, 2.5, 4800);
        assert!(quiet.iter().all(|s| *s == 0));
    }

    #[test]
    fn colours_parse() {
        assert_eq!(Rgb::parse("red").unwrap(), Rgb(200, 40, 40));
        assert_eq!(Rgb::parse("#102030").unwrap(), Rgb(16, 32, 48));
        assert!(Rgb::parse("nope").is_err());
        let (y, _, _) = Rgb(255, 255, 255).to_ycbcr();
        assert_eq!(y, 235);
    }
}
