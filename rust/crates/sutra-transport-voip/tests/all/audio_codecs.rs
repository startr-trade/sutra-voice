use sutra_transport_voip::audio::*;

use super::support::tone;

#[test]
fn mulaw_known_values_and_round_trip_error_is_bounded() {
    assert_eq!(mulaw_encode(0), 0xFF);
    assert_eq!(mulaw_decode(0xFF), 0);
    assert_eq!(mulaw_decode(0x00), -32124);
    assert_eq!(mulaw_decode(0x80), 32124);
    for sample in (-32000i32..32000).step_by(97) {
        let s = sample as i16;
        let back = mulaw_decode(mulaw_encode(s));
        let error = (i32::from(back) - sample).abs();
        assert!(error <= (sample.abs() / 16).max(8), "{sample} -> {back}");
    }
}

#[test]
fn alaw_known_values_and_round_trip_error_is_bounded() {
    assert_eq!(alaw_encode(0), 0xD5);
    assert_eq!(alaw_decode(0xD5), 8);
    assert_eq!(alaw_decode(0x55), -8);
    for sample in (-32000i32..32000).step_by(97) {
        let s = sample as i16;
        let back = alaw_decode(alaw_encode(s));
        let error = (i32::from(back) - sample).abs();
        assert!(error <= (sample.abs() / 16).max(16), "{sample} -> {back}");
    }
}

#[test]
fn pcm16_is_little_endian() {
    assert_eq!(pcm16_to_le(&[0x0102, -2]), vec![0x02, 0x01, 0xFE, 0xFF]);
    assert_eq!(
        pcm16_from_le(&[0x02, 0x01, 0xFE, 0xFF, 0x7]),
        vec![0x0102, -2]
    );
}

#[test]
fn wav_wrap_unwrap_round_trips_and_reads_g711_and_stereo() {
    let samples = tone(440.0, 50, 8000, 9000.0);
    let wav = wav_wrap(&samples, 8000);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(wav.len(), 44 + samples.len() * 2);
    assert_eq!(wav_unwrap(&wav).unwrap(), (samples.clone(), 8000));

    // A µ-law WAV (format 7) with an extra chunk before data.
    let mut mu = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    mu.extend_from_slice(&18u32.to_le_bytes());
    mu.extend_from_slice(&7u16.to_le_bytes());
    mu.extend_from_slice(&1u16.to_le_bytes());
    mu.extend_from_slice(&8000u32.to_le_bytes());
    mu.extend_from_slice(&8000u32.to_le_bytes());
    mu.extend_from_slice(&1u16.to_le_bytes());
    mu.extend_from_slice(&8u16.to_le_bytes());
    mu.extend_from_slice(&0u16.to_le_bytes());
    mu.extend_from_slice(b"LIST");
    mu.extend_from_slice(&3u32.to_le_bytes());
    mu.extend_from_slice(&[1, 2, 3, 0]);
    mu.extend_from_slice(b"data");
    mu.extend_from_slice(&2u32.to_le_bytes());
    mu.extend_from_slice(&[0xFF, 0x00]);
    assert_eq!(wav_unwrap(&mu).unwrap(), (vec![0, -32124], 8000));

    // Stereo PCM down-mixes by averaging.
    let mut stereo = wav_wrap(&[100, 300, -50, -150], 16000);
    stereo[22] = 2; // channels
    stereo[32] = 4; // block align
    assert_eq!(wav_unwrap(&stereo).unwrap(), (vec![200, -100], 16000));

    assert!(wav_unwrap(b"not a wav").is_err());
}

#[test]
fn media_types_parse_and_render() {
    assert_eq!(
        AudioFormat::parse("audio/L16;rate=16000"),
        Some(AudioFormat::l16(16000))
    );
    assert_eq!(
        AudioFormat::parse(" Audio/PCMU ").unwrap().encoding,
        AudioEncoding::Pcmu
    );
    assert_eq!(AudioFormat::parse("audio/pcma").unwrap().rate, 8000);
    assert_eq!(
        AudioFormat::parse("audio/x-wav").unwrap().encoding,
        AudioEncoding::Wav
    );
    assert_eq!(AudioFormat::parse("audio/ogg"), None);
    assert_eq!(AudioFormat::l16(8000).media_type(), "audio/L16;rate=8000");
    assert_eq!(
        AudioFormat::parse("audio/pcmu").unwrap().media_type(),
        "audio/PCMU"
    );
}

#[test]
fn linear_resampler_scales_length_and_preserves_tone() {
    let input = tone(400.0, 100, 8000, 10000.0);
    let up = resample_linear(&input, 8000, 16000);
    assert_eq!(up.len(), input.len() * 2);
    // Every even output sample is an input sample; odd ones interpolate.
    assert_eq!(up[0], input[0]);
    assert_eq!(up[10], input[5]);
    assert_eq!(
        up[11],
        ((f64::from(input[5]) + f64::from(input[6])) / 2.0).round() as i16
    );
    let down = resample_linear(&up, 16000, 8000);
    assert_eq!(down, input);
    let odd = resample_linear(&input, 8000, 24000);
    assert_eq!(odd.len(), input.len() * 3);
    assert_eq!(resample_linear(&input, 8000, 8000), input);
    assert!(resample_linear(&[], 8000, 16000).is_empty());
    // Rate conversion through a WAV body.
    let wav = wav_wrap(&input, 8000);
    let converted =
        convert_to_rate(&wav, &AudioFormat::parse("audio/wav").unwrap(), 16000).unwrap();
    assert_eq!(converted, up);
}
