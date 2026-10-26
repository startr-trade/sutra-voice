//! `sutra-webrtc-participant` — a SCRIPTED participant for tests and demos, not a client.
//!
//! Joins a LiveKit room and publishes a synthetic microphone (speech-like bursts at `--tone` Hz
//! inside the `--talk` stretches, digital silence outside, 48 kHz mono) and a synthetic camera
//! (`--color` with a moving white bar, `--size` at `--fps`), with its role in the participant
//! attribute the worker reads. Leaves after `--duration` seconds.
//!
//! ```text
//! sutra-webrtc-participant --room standup --identity alex --name "Alex" --role staff \
//!     --talk "1.0-3.0,6.5-8.5" --duration 12 --tone 160 --color teal --fps 15 --size 320x240
//! ```
//!
//! Environment: `LIVEKIT_URL`, and `SUTRA_WORKER_API_KEY` / `SUTRA_WORKER_API_SECRET` as
//! references (`env:NAME`, `file:/path`).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::prelude::{
    AudioFrame, AudioSourceOptions, I420Buffer, RtcAudioSource, RtcVideoSource, VideoFrame,
    VideoResolution, VideoRotation,
};
use livekit::webrtc::video_source::native::NativeVideoSource;
use sutra_webrtc_core::synth::{fill_i420, script_samples, Rgb, TalkScript};
use sutra_webrtc_worker::{init_logging, mint_token, secret_setting, setting, TokenRequest};

const RATE: u32 = 48_000;

struct Args {
    room: String,
    identity: String,
    name: String,
    role: Option<String>,
    role_attribute: String,
    talk: TalkScript,
    duration: f64,
    tone: f64,
    colour: Rgb,
    fps: f64,
    width: u32,
    height: u32,
}

fn parse_args() -> Result<Args, String> {
    let mut values = std::collections::HashMap::new();
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument '{flag}'"))?
            .to_string();
        let value = raw.next().ok_or_else(|| format!("--{key} needs a value"))?;
        values.insert(key, value);
    }
    let get = |key: &str, env: &str| values.get(key).cloned().or_else(|| setting(env));
    let number = |key: &str, env: &str, default: f64| -> Result<f64, String> {
        match get(key, env) {
            None => Ok(default),
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n > 0.0)
                .ok_or_else(|| format!("--{key}: '{v}' is not a positive number")),
        }
    };
    let size = get("size", "PARTICIPANT_SIZE").unwrap_or_else(|| "320x240".into());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|(w, h)| (16..=1920).contains(w) && (16..=1080).contains(h))
        .ok_or_else(|| format!("--size: '{size}' is not <width>x<height>"))?;
    let identity = get("identity", "PARTICIPANT_IDENTITY").ok_or("--identity is required")?;
    Ok(Args {
        room: get("room", "SUTRA_WORKER_ROOM").ok_or("--room is required")?,
        name: get("name", "PARTICIPANT_NAME").unwrap_or_else(|| identity.clone()),
        identity,
        role: get("role", "PARTICIPANT_ROLE"),
        role_attribute: get("role-attribute", "SUTRA_WORKER_ROLE_ATTRIBUTE")
            .unwrap_or_else(|| "role".into()),
        talk: TalkScript::parse(&get("talk", "PARTICIPANT_TALK").unwrap_or_default())?,
        duration: number("duration", "PARTICIPANT_DURATION", 20.0)?,
        tone: number("tone", "PARTICIPANT_TONE", 180.0)?,
        colour: Rgb::parse(&get("color", "PARTICIPANT_COLOR").unwrap_or_else(|| "blue".into()))?,
        fps: number("fps", "PARTICIPANT_FPS", 15.0)?.min(60.0),
        width: width & !1,
        height: height & !1,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;
    let url = setting("LIVEKIT_URL").unwrap_or_else(|| "ws://livekit:7880".into());
    let api_key = secret_setting("SUTRA_WORKER_API_KEY")?;
    let api_secret = secret_setting("SUTRA_WORKER_API_SECRET")?;
    let attributes = args
        .role
        .iter()
        .map(|role| (args.role_attribute.clone(), role.clone()))
        .collect();
    let token = mint_token(&TokenRequest {
        api_key: &api_key,
        api_secret: &api_secret,
        room: &args.room,
        identity: &args.identity,
        name: &args.name,
        attributes,
        can_subscribe: false,
        can_publish: true,
    })?;
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    let (room, _events) = Room::connect(&url, &token, options)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    tracing::info!(identity = %args.identity, "joined");

    let audio = NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 100);
    let mic =
        LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(audio.clone()));
    let video = NativeVideoSource::new(
        VideoResolution {
            width: args.width,
            height: args.height,
        },
        false,
    );
    let camera =
        LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(video.clone()));
    let participant = room.local_participant();
    participant
        .publish_track(
            LocalTrack::Audio(mic),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish microphone: {e}"))?;
    participant
        .publish_track(
            LocalTrack::Video(camera),
            TrackPublishOptions {
                source: TrackSource::Camera,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish camera: {e}"))?;
    tracing::info!(talk_secs = args.talk.total_secs(), "publishing");

    let joined = Instant::now();
    let deadline = joined + Duration::from_secs_f64(args.duration);
    let talk = args.talk.clone();
    let tone = args.tone;
    let audio_task = tokio::spawn(async move {
        // 10 ms frames on a 10 ms clock, generated by sample count so the script stays exact.
        let chunk = (RATE / 100) as usize;
        let mut sent: u64 = 0;
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        while Instant::now() < deadline {
            tick.tick().await;
            let samples = script_samples(&talk, tone, RATE, sent as f64 / f64::from(RATE), chunk);
            sent += chunk as u64;
            let frame = AudioFrame {
                data: samples.into(),
                sample_rate: RATE,
                num_channels: 1,
                samples_per_channel: chunk as u32,
            };
            if audio.capture_frame(&frame).await.is_err() {
                break;
            }
        }
    });
    let (width, height, colour, fps) = (args.width, args.height, args.colour, args.fps);
    let video_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while Instant::now() < deadline {
            tick.tick().await;
            let mut buffer = I420Buffer::new(width, height);
            let (stride_y, stride_u, stride_v) = buffer.strides();
            let bar = (joined.elapsed().as_secs_f32() * 0.25).fract();
            let (y, u, v) = buffer.data_mut();
            fill_i420(
                width,
                height,
                (y, stride_y),
                (u, stride_u),
                (v, stride_v),
                colour,
                bar,
            );
            video.capture_frame(&VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: joined.elapsed().as_micros() as i64,
                frame_metadata: None,
                buffer,
            });
        }
    });
    let _ = audio_task.await;
    let _ = video_task.await;
    let _ = room.close().await;
    tracing::info!(identity = %args.identity, "left");
    Ok(())
}
