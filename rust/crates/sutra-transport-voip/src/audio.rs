//! Audio formats and conversions, pure Rust: PCM16 little-endian, G.711 µ-law / A-law, WAV
//! wrap/unwrap and linear-interpolated resampling.
//!
//! The media server transcodes the call leg's negotiated codec at the edge and hands this
//! transport linear audio (or G.711 for gateways that cannot transcode), so these are the only
//! formats the transport ever touches. Sutra names them by media type: `audio/L16;rate=<hz>`,
//! `audio/PCMU`, `audio/PCMA` (8 kHz unless a `rate` parameter says otherwise) and `audio/wav`.

/// How the samples of an [`AudioFormat`] are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncoding {
    /// Signed 16-bit linear PCM, little-endian, mono.
    L16,
    /// G.711 µ-law, one byte per sample.
    Pcmu,
    /// G.711 A-law, one byte per sample.
    Pcma,
    /// A RIFF/WAVE container; the rate and encoding are read from its header.
    Wav,
}

/// A media type naming an audio encoding and its sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub encoding: AudioEncoding,
    /// Samples per second. For [`AudioEncoding::Wav`] this is a default only; the container
    /// header wins.
    pub rate: u32,
}

/// The telephony default rate.
pub const DEFAULT_RATE: u32 = 8000;

impl AudioFormat {
    pub const fn l16(rate: u32) -> AudioFormat {
        AudioFormat {
            encoding: AudioEncoding::L16,
            rate,
        }
    }

    /// Parse a media type: `audio/L16;rate=16000`, `audio/pcm`, `audio/PCMU`, `audio/PCMA`,
    /// `audio/x-mulaw`, `audio/x-alaw`, `audio/wav`, `audio/x-wav`, `audio/wave`. Case and
    /// whitespace are ignored; an unknown media type is `None`.
    pub fn parse(media_type: &str) -> Option<AudioFormat> {
        let lowered = media_type.trim().to_ascii_lowercase();
        let mut parts = lowered.split(';');
        let essence = parts.next()?.trim();
        let mut rate = None;
        for parameter in parts {
            if let Some((key, value)) = parameter.split_once('=') {
                if key.trim() == "rate" {
                    rate = value.trim().trim_matches('"').parse::<u32>().ok();
                }
            }
        }
        let encoding = match essence {
            "audio/l16" | "audio/pcm" | "audio/x-pcm" => AudioEncoding::L16,
            "audio/pcmu" | "audio/x-mulaw" | "audio/basic" => AudioEncoding::Pcmu,
            "audio/pcma" | "audio/x-alaw" => AudioEncoding::Pcma,
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => AudioEncoding::Wav,
            _ => return None,
        };
        let rate = rate.filter(|r| *r > 0).unwrap_or(DEFAULT_RATE);
        Some(AudioFormat { encoding, rate })
    }

    /// The canonical media type string.
    pub fn media_type(&self) -> String {
        match self.encoding {
            AudioEncoding::L16 => format!("audio/L16;rate={}", self.rate),
            AudioEncoding::Pcmu if self.rate == DEFAULT_RATE => "audio/PCMU".to_string(),
            AudioEncoding::Pcmu => format!("audio/PCMU;rate={}", self.rate),
            AudioEncoding::Pcma if self.rate == DEFAULT_RATE => "audio/PCMA".to_string(),
            AudioEncoding::Pcma => format!("audio/PCMA;rate={}", self.rate),
            AudioEncoding::Wav => "audio/wav".to_string(),
        }
    }

    /// Bytes per sample on the wire (WAV reports 2, its PCM payload width).
    pub fn bytes_per_sample(&self) -> usize {
        match self.encoding {
            AudioEncoding::Pcmu | AudioEncoding::Pcma => 1,
            AudioEncoding::L16 | AudioEncoding::Wav => 2,
        }
    }
}

/// Decode little-endian PCM16; a trailing odd byte is dropped.
pub fn pcm16_from_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Encode samples as little-endian PCM16.
pub fn pcm16_to_le(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

const MULAW_BIAS: i32 = 0x84;
const MULAW_CLIP: i32 = 32635;

/// G.711 µ-law encode one linear sample.
pub fn mulaw_encode(sample: i16) -> u8 {
    let mut magnitude = i32::from(sample);
    let sign = if magnitude < 0 {
        magnitude = -magnitude;
        0x80
    } else {
        0
    };
    magnitude = magnitude.min(MULAW_CLIP) + MULAW_BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && magnitude & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (magnitude >> (exponent + 3)) & 0x0F;
    !((sign | (exponent << 4) | mantissa) as u8)
}

/// G.711 µ-law decode one byte.
pub fn mulaw_decode(byte: u8) -> i16 {
    let value = !byte;
    let sign = value & 0x80;
    let exponent = i32::from((value >> 4) & 0x07);
    let mantissa = i32::from(value & 0x0F);
    let magnitude = (((mantissa << 3) + MULAW_BIAS) << exponent) - MULAW_BIAS;
    if sign != 0 {
        -magnitude as i16
    } else {
        magnitude as i16
    }
}

const ALAW_SEGMENT_END: [i32; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

/// G.711 A-law encode one linear sample.
pub fn alaw_encode(sample: i16) -> u8 {
    let mut value = i32::from(sample) >> 3;
    let mask = if value >= 0 {
        0xD5
    } else {
        value = -value - 1;
        0x55
    };
    let segment = ALAW_SEGMENT_END
        .iter()
        .position(|end| value <= *end)
        .unwrap_or(8);
    if segment >= 8 {
        return (0x7F ^ mask) as u8;
    }
    let mut encoded = (segment as i32) << 4;
    encoded |= if segment < 2 {
        (value >> 1) & 0x0F
    } else {
        (value >> segment) & 0x0F
    };
    (encoded ^ mask) as u8
}

/// G.711 A-law decode one byte.
pub fn alaw_decode(byte: u8) -> i16 {
    let value = byte ^ 0x55;
    let mut magnitude = i32::from(value & 0x0F) << 4;
    let segment = i32::from((value & 0x70) >> 4);
    match segment {
        0 => magnitude += 8,
        1 => magnitude += 0x108,
        _ => {
            magnitude += 0x108;
            magnitude <<= segment - 1;
        }
    }
    if value & 0x80 != 0 {
        magnitude as i16
    } else {
        -magnitude as i16
    }
}

/// Wrap mono PCM16 samples in a canonical 44-byte-header WAV container.
pub fn wav_wrap(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&pcm16_to_le(samples));
    out
}

/// Unwrap a WAV container into mono PCM16 samples and their rate. Accepts PCM 16-bit and
/// 8-bit, G.711 µ-law (format 7) and A-law (format 6), and `WAVE_FORMAT_EXTENSIBLE` carrying
/// PCM; multi-channel input is down-mixed by averaging.
pub fn wav_unwrap(bytes: &[u8]) -> Result<(Vec<i16>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE container".to_string());
    }
    let mut cursor = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let len = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        let body_end = body_start.saturating_add(len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("WAV fmt chunk is truncated".to_string());
            }
            let mut tag = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]).max(1);
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if tag == 0xFFFE && body.len() >= 26 {
                tag = u16::from_le_bytes([body[24], body[25]]);
            }
            format = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            let Some((tag, channels, rate, bits)) = format else {
                return Err("WAV data chunk precedes its fmt chunk".to_string());
            };
            let interleaved: Vec<i16> = match (tag, bits) {
                (1, 16) => pcm16_from_le(body),
                (1, 8) => body.iter().map(|b| (i16::from(*b) - 128) << 8).collect(),
                (7, 8) => body.iter().map(|b| mulaw_decode(*b)).collect(),
                (6, 8) => body.iter().map(|b| alaw_decode(*b)).collect(),
                _ => {
                    return Err(format!(
                        "unsupported WAV encoding (format tag {tag}, {bits} bits)"
                    ))
                }
            };
            return Ok((downmix(&interleaved, channels as usize), rate.max(1)));
        }
        // Chunks are word-aligned.
        cursor = body_start + len + (len & 1);
    }
    Err("WAV container has no data chunk".to_string())
}

fn downmix(interleaved: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

/// Resample by linear interpolation. Identity when the rates match.
pub fn resample_linear(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return samples.to_vec();
    }
    let out_len = ((samples.len() as u64 * u64::from(to_rate)) / u64::from(from_rate)) as usize;
    let step = f64::from(from_rate) / f64::from(to_rate);
    let last = samples.len() - 1;
    (0..out_len)
        .map(|i| {
            let position = i as f64 * step;
            let index = position.floor() as usize;
            if index >= last {
                return samples[last];
            }
            let fraction = position - index as f64;
            let a = f64::from(samples[index]);
            let b = f64::from(samples[index + 1]);
            (a + (b - a) * fraction).round() as i16
        })
        .collect()
}

/// Decode bytes in `format` to mono PCM16 samples and their rate.
pub fn decode(bytes: &[u8], format: &AudioFormat) -> Result<(Vec<i16>, u32), String> {
    match format.encoding {
        AudioEncoding::L16 => Ok((pcm16_from_le(bytes), format.rate)),
        AudioEncoding::Pcmu => Ok((
            bytes.iter().map(|b| mulaw_decode(*b)).collect(),
            format.rate,
        )),
        AudioEncoding::Pcma => Ok((bytes.iter().map(|b| alaw_decode(*b)).collect(), format.rate)),
        AudioEncoding::Wav => wav_unwrap(bytes),
    }
}

/// Encode mono PCM16 samples (already at `format.rate`) into `format`.
pub fn encode(samples: &[i16], format: &AudioFormat) -> Vec<u8> {
    match format.encoding {
        AudioEncoding::L16 => pcm16_to_le(samples),
        AudioEncoding::Pcmu => samples.iter().map(|s| mulaw_encode(*s)).collect(),
        AudioEncoding::Pcma => samples.iter().map(|s| alaw_encode(*s)).collect(),
        AudioEncoding::Wav => wav_wrap(samples, format.rate),
    }
}

/// Decode `bytes` in `from`, resample, and return samples at `to_rate`.
pub fn convert_to_rate(bytes: &[u8], from: &AudioFormat, to_rate: u32) -> Result<Vec<i16>, String> {
    let (samples, rate) = decode(bytes, from)?;
    Ok(resample_linear(&samples, rate, to_rate))
}
