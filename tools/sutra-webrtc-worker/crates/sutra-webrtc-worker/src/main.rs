//! `sutra-webrtc-worker` — the meeting bot.
//!
//! Joins one LiveKit room as a visible participant, subscribes to every other participant's
//! audio and video, and — while anyone else is in the room — holds ONE media-stream session
//! with the engine (`GET /voip/stream`, `"streams": "tagged"`): each participant is announced
//! with a session index, a display label and its declared role (never its identity), its audio
//! is forwarded as PCM16 mono at the session rate, and its video is sampled at the rate the
//! engine's `policy` sets (plus the extra frames the engine `sample`s) and sent as JPEG. Audio the
//! engine sends is spoken into the room on the bot's own track (the consent announcement).
//!
//! The session ends when the room has been empty for the grace period, the room closes, or the
//! engine hangs up; the bot then waits for the next meeting (or exits, with
//! `SUTRA_WORKER_ONCE=true`).

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use livekit::options::TrackPublishOptions;
use livekit::participant::ParticipantKind;
use livekit::prelude::*;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::{
    AudioFrame, AudioSourceOptions, BoxVideoFrame, RtcAudioSource, VideoBuffer,
};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use sutra_webrtc_core::jpeg::{self, I420};
use sutra_webrtc_core::protocol::{
    self, EngineFrame, FrameHeader, StartInfo, VideoPolicy, FLAG_REQUESTED, KIND_AUDIO, KIND_VIDEO,
};
use sutra_webrtc_core::sampler::{FrameSampler, SampleKind};
use sutra_webrtc_worker::{
    flag, init_logging, mint_token, number, required, secret_setting, setting,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

struct Config {
    livekit_url: String,
    room: String,
    api_key: String,
    api_secret: String,
    media_token: String,
    engine_url: String,
    called: String,
    identity: String,
    name: String,
    audio_rate: u32,
    role_attribute: String,
    empty_grace: Duration,
    once: bool,
}

impl Config {
    fn from_env() -> Result<Config, String> {
        let audio_rate: u32 = number("SUTRA_WORKER_AUDIO_RATE", 16_000)?;
        if ![8_000, 16_000, 24_000, 48_000].contains(&audio_rate) {
            return Err(format!(
                "SUTRA_WORKER_AUDIO_RATE: {audio_rate} is not 8000, 16000, 24000 or 48000"
            ));
        }
        Ok(Config {
            livekit_url: setting("LIVEKIT_URL").unwrap_or_else(|| "ws://livekit:7880".into()),
            room: required("SUTRA_WORKER_ROOM")?,
            api_key: secret_setting("SUTRA_WORKER_API_KEY")?,
            api_secret: secret_setting("SUTRA_WORKER_API_SECRET")?,
            media_token: secret_setting("SUTRA_WORKER_MEDIA_TOKEN")?,
            engine_url: setting("SUTRA_WORKER_ENGINE_URL")
                .unwrap_or_else(|| "ws://engine:8080/voip/stream".into()),
            called: required("SUTRA_WORKER_CALLED")?,
            identity: setting("SUTRA_WORKER_IDENTITY")
                .unwrap_or_else(|| "sutra-meeting-bot".into()),
            name: setting("SUTRA_WORKER_NAME")
                .unwrap_or_else(|| "Meeting assistant (recording)".into()),
            audio_rate,
            role_attribute: setting("SUTRA_WORKER_ROLE_ATTRIBUTE").unwrap_or_else(|| "role".into()),
            empty_grace: Duration::from_millis(number("SUTRA_WORKER_EMPTY_GRACE_MS", 3_000u64)?),
            once: flag("SUTRA_WORKER_ONCE", false)?,
        })
    }

    /// The engine URL with the media token (percent-encoded).
    fn stream_url(&self) -> String {
        let separator = if self.engine_url.contains('?') {
            '&'
        } else {
            '?'
        };
        let token: String = self
            .media_token
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("{}{separator}token={token}", self.engine_url)
    }
}

/// What the media tasks need to reach the current session.
struct SessionLink {
    out: mpsc::Sender<Message>,
    started: Instant,
    /// Participant SID → session index.
    indices: HashMap<String, u16>,
}

#[derive(Default)]
struct Shared {
    link: Mutex<Option<SessionLink>>,
    dropped: AtomicU64,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<SessionLink>> {
        self.link.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The sender, index and session clock for `sid`, while it is in the session.
    fn route(&self, sid: &str) -> Option<(mpsc::Sender<Message>, u16, Instant)> {
        let guard = self.lock();
        let link = guard.as_ref()?;
        let index = *link.indices.get(sid)?;
        Some((link.out.clone(), index, link.started))
    }

    fn send(&self, out: &mpsc::Sender<Message>, message: Message) {
        if out.try_send(message).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                tracing::warn!(dropped, "engine socket backlog: frames dropped");
            }
        }
    }
}

/// A remote participant in the room.
struct Present {
    label_source: String,
    role: Option<String>,
    excluded: bool,
    latest: Arc<Mutex<Option<Arc<BoxVideoFrame>>>>,
    encoding: Arc<AtomicBool>,
    sampler: FrameSampler,
    tasks: HashMap<String, tokio::task::JoinHandle<()>>,
}

enum EngineIn {
    Frame(EngineFrame),
    Audio(Vec<i16>),
    Closed,
}

enum SpeakItem {
    Audio(u64, Vec<i16>),
    Mark(u64, String),
}

/// The bot's own audio track, published on the first audio the engine sends.
struct Speaker {
    source: NativeAudioSource,
    queue: mpsc::UnboundedSender<SpeakItem>,
    generation: Arc<AtomicU64>,
}

struct Session {
    call_id: String,
    started: Instant,
    next_index: u16,
    policy: VideoPolicy,
    engine: mpsc::Receiver<EngineIn>,
    out: mpsc::Sender<Message>,
    writer: tokio::task::JoinHandle<()>,
    speaker: Option<Speaker>,
}

impl Session {
    fn at_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

enum RoomExit {
    Shutdown,
    Done,
    /// Rejoin; `true` = wait for the room to empty before the next session.
    Rejoin(bool),
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("configuration: {e}");
            return ExitCode::from(2);
        }
    };
    tracing::info!(room = %config.room, url = %config.livekit_url, "sutra-webrtc-worker starting");
    let mut wait_empty = false;
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_room(&config, wait_empty).await {
            Ok(RoomExit::Shutdown) => return ExitCode::SUCCESS,
            Ok(RoomExit::Done) => return ExitCode::SUCCESS,
            Ok(RoomExit::Rejoin(wait)) => {
                wait_empty = wait;
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!("room: {e}; retrying in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn participant_excluded(participant: &RemoteParticipant) -> bool {
    // A recorder is not a meeting participant.
    participant.kind() == ParticipantKind::Egress
}

async fn run_room(config: &Config, mut wait_empty: bool) -> Result<RoomExit, String> {
    let token = mint_token(&sutra_webrtc_worker::TokenRequest {
        api_key: &config.api_key,
        api_secret: &config.api_secret,
        room: &config.room,
        identity: &config.identity,
        name: &config.name,
        attributes: Vec::new(),
        can_subscribe: true,
        can_publish: true,
    })?;
    let (room, mut events) = Room::connect(&config.livekit_url, &token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    tracing::info!("joined the room");
    let room = Arc::new(room);
    let shared = Arc::new(Shared::default());
    let mut present: HashMap<String, Present> = HashMap::new();
    for participant in room.remote_participants().into_values() {
        add_present(config, &mut present, &participant);
    }
    let mut session: Option<Session> = None;
    let mut empty_since: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = Box::pin(shutdown_signal());

    let occupied = |present: &HashMap<String, Present>| present.values().any(|p| !p.excluded);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                if let Some(s) = session.take() {
                    end_session(&shared, s, "worker-shutdown").await;
                }
                let _ = room.close().await;
                return Ok(RoomExit::Shutdown);
            }
            event = events.recv() => {
                let Some(event) = event else {
                    if let Some(s) = session.take() {
                        end_session(&shared, s, "room-closed").await;
                    }
                    return Ok(if config.once { RoomExit::Done } else { RoomExit::Rejoin(false) });
                };
                match event {
                    RoomEvent::ParticipantConnected(participant) => {
                        let sid = participant.sid().to_string();
                        add_present(config, &mut present, &participant);
                        if let Some(s) = session.as_mut() {
                            announce(&shared, s, &mut present, &sid);
                        }
                        if occupied(&present) {
                            empty_since = None;
                        }
                    }
                    RoomEvent::ParticipantDisconnected(participant) => {
                        let sid = participant.sid().to_string();
                        if let Some(gone) = present.remove(&sid) {
                            for task in gone.tasks.values() {
                                task.abort();
                            }
                        }
                        if let Some(s) = session.as_mut() {
                            let in_session = shared
                                .lock()
                                .as_mut()
                                .and_then(|l| l.indices.remove(&sid))
                                .is_some();
                            if in_session {
                                let text = protocol::participant_left(&sid, "left", s.at_ms());
                                let _ = s.out.send(Message::Text(text.into())).await;
                                tracing::info!(participant = %sid, "participant left");
                            }
                        }
                        if !occupied(&present) {
                            empty_since = Some(Instant::now());
                            if wait_empty {
                                tracing::info!("the room emptied; ready for the next meeting");
                                wait_empty = false;
                            }
                        }
                    }
                    RoomEvent::TrackSubscribed { track, participant, .. } => {
                        let sid = participant.sid().to_string();
                        if !present.contains_key(&sid) {
                            add_present(config, &mut present, &participant);
                        }
                        let Some(entry) = present.get_mut(&sid) else { continue };
                        if entry.excluded {
                            continue;
                        }
                        let (kind, handle) = match &track {
                            RemoteTrack::Audio(audio) => (
                                "audio",
                                spawn_audio(Arc::clone(&shared), sid.clone(), audio.clone(), config.audio_rate),
                            ),
                            RemoteTrack::Video(video) => (
                                "video",
                                spawn_video(Arc::clone(&entry.latest), video.clone()),
                            ),
                        };
                        if let Some(old) = entry.tasks.insert(track.sid().to_string(), handle) {
                            old.abort();
                        }
                        tracing::info!(participant = %sid, kind, "track subscribed");
                        if let (Some(s), Some((out, _, _))) = (session.as_ref(), shared.route(&sid)) {
                            let text = protocol::track(&sid, kind, "subscribed", s.at_ms());
                            shared.send(&out, Message::Text(text.into()));
                        }
                    }
                    RoomEvent::TrackUnsubscribed { track, participant, .. } => {
                        let sid = participant.sid().to_string();
                        let kind = match &track {
                            RemoteTrack::Audio(_) => "audio",
                            RemoteTrack::Video(_) => "video",
                        };
                        if let Some(entry) = present.get_mut(&sid) {
                            if let Some(task) = entry.tasks.remove(&track.sid().to_string()) {
                                task.abort();
                            }
                            if kind == "video" {
                                *entry.latest.lock().unwrap_or_else(|p| p.into_inner()) = None;
                            }
                        }
                        if let (Some(s), Some((out, _, _))) = (session.as_ref(), shared.route(&sid)) {
                            let text = protocol::track(&sid, kind, "unsubscribed", s.at_ms());
                            shared.send(&out, Message::Text(text.into()));
                        }
                    }
                    RoomEvent::Disconnected { reason } => {
                        tracing::warn!(?reason, "disconnected from the room");
                        if let Some(s) = session.take() {
                            end_session(&shared, s, "room-closed").await;
                        }
                        return Ok(if config.once { RoomExit::Done } else { RoomExit::Rejoin(false) });
                    }
                    _ => {}
                }
            }
            incoming = async {
                match session.as_mut() {
                    Some(s) => s.engine.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(s) = session.as_mut() else { continue };
                match incoming.unwrap_or(EngineIn::Closed) {
                    EngineIn::Frame(EngineFrame::Policy(policy)) => {
                        let now = s.at_ms();
                        tracing::info!(enabled = policy.enabled, fps = policy.fps, max_width = policy.max_width, "video policy");
                        for p in present.values_mut() {
                            p.sampler.set_policy(&policy, now);
                        }
                        s.policy = policy;
                    }
                    EngineIn::Frame(EngineFrame::Sample { participant_id, reason }) => {
                        let now = s.at_ms();
                        let mut accepted = 0;
                        for (sid, p) in present.iter_mut() {
                            if participant_id.as_ref().is_none_or(|wanted| wanted == sid)
                                && p.sampler.request(now)
                            {
                                accepted += 1;
                            }
                        }
                        tracing::debug!(%reason, accepted, "sample requested");
                    }
                    EngineIn::Frame(EngineFrame::Clear) => {
                        if let Some(speaker) = &s.speaker {
                            speaker.generation.fetch_add(1, Ordering::SeqCst);
                            speaker.source.clear_buffer();
                        }
                    }
                    EngineIn::Frame(EngineFrame::Mark(name)) => match &s.speaker {
                        Some(speaker) => {
                            let generation = speaker.generation.load(Ordering::SeqCst);
                            let _ = speaker.queue.send(SpeakItem::Mark(generation, name));
                        }
                        None => {
                            // Nothing queued: the mark is reached at once.
                            let _ = s.out.send(Message::Text(protocol::mark(&name).into())).await;
                        }
                    },
                    EngineIn::Frame(EngineFrame::Hangup) => {
                        tracing::info!(call_id = %s.call_id, "the engine hung up");
                        if let Some(s) = session.take() {
                            end_session(&shared, s, "hangup").await;
                        }
                        let _ = room.close().await;
                        return Ok(if config.once { RoomExit::Done } else { RoomExit::Rejoin(true) });
                    }
                    EngineIn::Frame(EngineFrame::Other(event)) => {
                        tracing::debug!(%event, "ignored engine frame");
                    }
                    EngineIn::Audio(samples) => {
                        if s.speaker.is_none() {
                            match publish_speaker(&room, config.audio_rate, s.out.clone()).await {
                                Ok(speaker) => s.speaker = Some(speaker),
                                Err(e) => {
                                    tracing::warn!("the bot's audio track could not be published: {e}");
                                    continue;
                                }
                            }
                        }
                        if let Some(speaker) = &s.speaker {
                            let generation = speaker.generation.load(Ordering::SeqCst);
                            let _ = speaker.queue.send(SpeakItem::Audio(generation, samples));
                        }
                    }
                    EngineIn::Closed => {
                        tracing::warn!(call_id = %s.call_id, "the engine socket closed mid-session");
                        if let Some(s) = session.take() {
                            *shared.lock() = None;
                            s.writer.abort();
                        }
                        if config.once {
                            let _ = room.close().await;
                            return Ok(RoomExit::Done);
                        }
                        wait_empty = occupied(&present);
                    }
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                match session.as_mut() {
                    None => {
                        let due = retry_at.is_none_or(|at| now >= at);
                        if occupied(&present) && !wait_empty && due {
                            match open_session(config, &shared).await {
                                Ok(mut s) => {
                                    tracing::info!(call_id = %s.call_id, "session started");
                                    let sids: Vec<String> = present
                                        .iter()
                                        .filter(|(_, p)| !p.excluded)
                                        .map(|(sid, _)| sid.clone())
                                        .collect();
                                    for sid in sids {
                                        announce(&shared, &mut s, &mut present, &sid);
                                    }
                                    retry_at = None;
                                    empty_since = None;
                                    session = Some(s);
                                }
                                Err(e) => {
                                    tracing::warn!("engine session not opened: {e}; retrying in 5 s");
                                    retry_at = Some(now + Duration::from_secs(5));
                                }
                            }
                        }
                    }
                    Some(s) => {
                        if empty_since.is_some_and(|at| now.duration_since(at) >= config.empty_grace) {
                            empty_since = None;
                            if let Some(s) = session.take() {
                                end_session(&shared, s, "room-empty").await;
                            }
                            if config.once {
                                let _ = room.close().await;
                                return Ok(RoomExit::Done);
                            }
                            continue;
                        }
                        if s.policy.enabled {
                            sample_frames(&shared, s, &mut present);
                        }
                    }
                }
            }
        }
    }
}

fn add_present(
    config: &Config,
    present: &mut HashMap<String, Present>,
    participant: &RemoteParticipant,
) {
    let sid = participant.sid().to_string();
    if present.contains_key(&sid) {
        return;
    }
    let role = participant
        .attributes()
        .get(&config.role_attribute)
        .map(|r| r.trim().to_lowercase())
        .filter(|r| !r.is_empty());
    present.insert(
        sid,
        Present {
            label_source: participant.name(),
            role,
            excluded: participant_excluded(participant),
            latest: Arc::new(Mutex::new(None)),
            encoding: Arc::new(AtomicBool::new(false)),
            sampler: FrameSampler::new(),
            tasks: HashMap::new(),
        },
    );
}

/// Give `sid` a session index and announce it.
fn announce(
    shared: &Shared,
    session: &mut Session,
    present: &mut HashMap<String, Present>,
    sid: &str,
) {
    let Some(entry) = present.get_mut(sid) else {
        return;
    };
    if entry.excluded {
        return;
    }
    let index = {
        let mut guard = shared.lock();
        let Some(link) = guard.as_mut() else { return };
        if link.indices.contains_key(sid) {
            return;
        }
        let index = session.next_index;
        link.indices.insert(sid.to_string(), index);
        index
    };
    session.next_index = session.next_index.saturating_add(1);
    entry.sampler.set_policy(&session.policy, session.at_ms());
    let label = protocol::label_of(&entry.label_source, index);
    let text =
        protocol::participant_joined(sid, index, &label, entry.role.as_deref(), session.at_ms());
    shared.send(&session.out, Message::Text(text.into()));
    tracing::info!(participant = %sid, index, role = entry.role.as_deref().unwrap_or("-"), "participant announced");
}

async fn open_session(config: &Config, shared: &Arc<Shared>) -> Result<Session, String> {
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(config.stream_url()),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| format!("connect: {e}"))?;
    let (mut sink, mut stream) = socket.split();
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let call_id = format!("{}-{unix_ms}", config.room);
    let start = protocol::start(&StartInfo {
        call_id: call_id.clone(),
        stream_id: config.room.clone(),
        room: config.room.clone(),
        called: config.called.clone(),
        rate: config.audio_rate,
    });
    sink.send(Message::Text(start.into()))
        .await
        .map_err(|e| format!("start: {e}"))?;
    let started = Instant::now();
    let (out, mut queue) = mpsc::channel::<Message>(1024);
    let writer = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            let close = matches!(message, Message::Close(_));
            if sink.send(message).await.is_err() || close {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let (engine_tx, engine) = mpsc::channel::<EngineIn>(256);
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let forwarded = match frame {
                Ok(Message::Text(text)) => match protocol::parse_engine_text(text.as_str()) {
                    Ok(frame) => EngineIn::Frame(frame),
                    Err(e) => {
                        tracing::debug!("ignored engine text frame: {e}");
                        continue;
                    }
                },
                Ok(Message::Binary(bytes)) => EngineIn::Audio(protocol::pcm16_from_le(&bytes)),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            if engine_tx.send(forwarded).await.is_err() {
                return;
            }
        }
        let _ = engine_tx.send(EngineIn::Closed).await;
    });
    *shared.lock() = Some(SessionLink {
        out: out.clone(),
        started,
        indices: HashMap::new(),
    });
    Ok(Session {
        call_id,
        started,
        next_index: 1,
        policy: VideoPolicy::default(),
        engine,
        out,
        writer,
        speaker: None,
    })
}

async fn end_session(shared: &Shared, session: Session, reason: &str) {
    *shared.lock() = None;
    let _ = session
        .out
        .send(Message::Text(protocol::stop(reason).into()))
        .await;
    let _ = session.out.send(Message::Close(None)).await;
    drop(session.out);
    let _ = tokio::time::timeout(Duration::from_secs(3), session.writer).await;
    tracing::info!(call_id = %session.call_id, reason, "session ended");
}

fn spawn_audio(
    shared: Arc<Shared>,
    sid: String,
    track: RemoteAudioTrack,
    rate: u32,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = NativeAudioStream::new(track.rtc_track(), rate as i32, 1);
        let chunk = (rate / 50) as usize;
        let mut pending: Vec<i16> = Vec::with_capacity(chunk * 2);
        while let Some(frame) = stream.next().await {
            let Some((out, index, started)) = shared.route(&sid) else {
                pending.clear();
                continue;
            };
            pending.extend_from_slice(&frame.data);
            if pending.len() < chunk {
                continue;
            }
            let held_ms = pending.len() as u64 * 1000 / u64::from(rate);
            let at_ms = (started.elapsed().as_millis() as u64).saturating_sub(held_ms);
            let header = FrameHeader {
                kind: KIND_AUDIO,
                flags: 0,
                index,
                at_ms,
            };
            let frame = header.encode(&protocol::pcm16_le(&pending));
            shared.send(&out, Message::Binary(frame.into()));
            pending.clear();
        }
    })
}

fn spawn_video(
    latest: Arc<Mutex<Option<Arc<BoxVideoFrame>>>>,
    track: RemoteVideoTrack,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Only the newest decoded frame is kept; nothing is encoded here.
        let mut stream = NativeVideoStream::new(track.rtc_track());
        while let Some(frame) = stream.next().await {
            *latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(frame));
        }
    })
}

/// Send each participant's frame when its sampler says so (encoding off the event loop).
fn sample_frames(shared: &Arc<Shared>, session: &Session, present: &mut HashMap<String, Present>) {
    let now = session.at_ms();
    for (sid, p) in present.iter_mut() {
        let frame = p.latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(frame) = frame else { continue };
        if p.encoding.load(Ordering::SeqCst) {
            continue;
        }
        let Some(kind) = p.sampler.poll(now) else {
            continue;
        };
        let Some((out, index, _)) = shared.route(sid) else {
            continue;
        };
        let policy = session.policy.clone();
        let encoding = Arc::clone(&p.encoding);
        let shared = Arc::clone(shared);
        encoding.store(true, Ordering::SeqCst);
        tokio::task::spawn_blocking(move || {
            match encode_frame(&frame, &policy) {
                Ok(jpeg) => {
                    let header = FrameHeader {
                        kind: KIND_VIDEO,
                        flags: if kind == SampleKind::Requested {
                            FLAG_REQUESTED
                        } else {
                            0
                        },
                        index,
                        at_ms: now,
                    };
                    shared.send(&out, Message::Binary(header.encode(&jpeg).into()));
                }
                Err(e) => tracing::warn!(index, "frame not encoded: {e}"),
            }
            encoding.store(false, Ordering::SeqCst);
        });
    }
}

fn encode_frame(frame: &BoxVideoFrame, policy: &VideoPolicy) -> Result<Vec<u8>, String> {
    let mut i420 = frame.buffer.to_i420();
    let (width, height) = (i420.width(), i420.height());
    if width > policy.max_width {
        let scaled_height = ((u64::from(height) * u64::from(policy.max_width)) / u64::from(width))
            .max(2) as i32
            & !1;
        i420 = i420.scale(policy.max_width as i32 & !1, scaled_height);
    }
    let (stride_y, stride_u, stride_v) = i420.strides();
    let (y, u, v) = i420.data();
    jpeg::encode_i420(
        &I420 {
            width: i420.width(),
            height: i420.height(),
            y,
            stride_y,
            u,
            stride_u,
            v,
            stride_v,
        },
        policy.quality,
    )
    .map_err(|e| e.to_string())
}

/// Publish the bot's audio track and start the task that speaks what the engine sends.
async fn publish_speaker(
    room: &Room,
    rate: u32,
    out: mpsc::Sender<Message>,
) -> Result<Speaker, String> {
    let source = NativeAudioSource::new(AudioSourceOptions::default(), rate, 1, 200);
    let track =
        LocalAudioTrack::create_audio_track("assistant", RtcAudioSource::Native(source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("the bot's audio track is published");
    let (queue, mut items) = mpsc::unbounded_channel::<SpeakItem>();
    let generation = Arc::new(AtomicU64::new(0));
    let current = Arc::clone(&generation);
    let speaking = source.clone();
    tokio::spawn(async move {
        let chunk = (rate / 100) as usize;
        while let Some(item) = items.recv().await {
            match item {
                SpeakItem::Audio(at, samples) => {
                    for piece in samples.chunks(chunk) {
                        if current.load(Ordering::SeqCst) != at {
                            break;
                        }
                        let mut data = piece.to_vec();
                        data.resize(chunk, 0);
                        let frame = AudioFrame {
                            data: data.into(),
                            sample_rate: rate,
                            num_channels: 1,
                            samples_per_channel: chunk as u32,
                        };
                        if speaking.capture_frame(&frame).await.is_err() {
                            break;
                        }
                    }
                }
                SpeakItem::Mark(at, name) => {
                    if current.load(Ordering::SeqCst) == at {
                        let _ = out.send(Message::Text(protocol::mark(&name).into())).await;
                    }
                }
            }
        }
    });
    Ok(Speaker {
        source,
        queue,
        generation,
    })
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
