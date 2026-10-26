//! Conformance: `examples/video-meeting-assistant` end to end — a WebRTC media worker sits in a
//! LiveKit room as a participant and streams every other participant (audio + sampled video) to
//! the engine on one tagged media-stream socket; the engine records and transcribes each
//! participant separately and cuts each external participant's media into segments, which the
//! flows analyse.
//!
//! One docker network carries the whole session:
//!
//! | container | role |
//! |---|---|
//! | `sutra-voice-engine:dev` | the engine with the `video-stub` package, the consent prompt mounted, recordings to the object store below |
//! | `livekit/livekit-server`, pinned by digest | the room (dev mode, single node, no Redis) |
//! | `sutra-webrtc-worker:dev` (worker) | the bot: joins the room, streams every participant to the engine (`make worker-image`) |
//! | `sutra-webrtc-worker:dev` (participant ×2) | scripted participants: a synthetic microphone on a talk script and a synthetic camera; one `staff`, one `external` |
//! | MinIO (community build, pinned by digest) | the S3-compatible object store the recordings are sealed into |
//! | the examples' stub model server | the Realtime transcription sessions (one per participant) and the native Gemini `generateContent` analysis the flow calls |
//! | PostgreSQL | the engine's database (audit_event) and the package's stores |
//!
//! WHAT IS PROVEN, from one meeting: the bot joins and the session starts; both participants'
//! streams arrive attributed to the right participant (each transcript row comes from that
//! participant's own track, and each track got its own transcription session); frames are sampled
//! at the configured rate (1 fps) plus extra frames at turn boundaries; one sealed recording per
//! participant is in the bucket and its SHA-256 is the audited one; each segment of the external
//! participant is written to the bucket (sealed audio and frames) and reaches the flow as media
//! references; the flow's policy (DMN) analyses it, the model reads the media through the `llm`
//! channel's media-store (the stub reports the frames and audio it received), and the flow keeps
//! only the fields it asked for. The staff participant is never cut into segments and never
//! analysed (no opt-in); the consent announcement was played before anything was recorded; the
//! session ends cleanly (the worker exits 0).

use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;
use sutra_objectstore::ObjectStore;
use testcontainers::core::{AccessMode, IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::support::compose;
use crate::support::engine::{self, EngineBuilder, EngineHandle, PgFixture};
use crate::support::util;
use crate::tc_voice_assistant::{docker, wait_for};

/// Fixture names start `videomeet` so every container lands on the one suite network.
const SUITE: &str = "videomeet";
/// livekit-server v1.13.9, pinned by digest — the same image the example's compose.yaml pins.
const LIVEKIT: &str =
    "livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154";
const MINIO: &str =
    "pgsty/minio@sha256:b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372";
const WORKER_IMAGE: &str = "sutra-webrtc-worker:dev";
const MEDIA_TOKEN: &str = "dev-only-media-token";
const STORE_ACCESS: &str = "videomeet-e2e-access";
const STORE_SECRET: &str = "videomeet-e2e-secret-key";
const KEK: &str = "videomeet-e2e-recordings-kek";
const ROOM: &str = "standup";
/// Each participant talks twice; the stub answers each stretch with the next of these lines
/// (per transcription session, so per participant).
const LINES: [&str; 2] = [
    "Good morning, everyone. Let's go through the launch plan for the new booking page.",
    "Thanks. The design is ready; the open question is the release date.",
];
/// How long each scripted participant stays (s).
const DURATION_S: u64 = 16;

struct VideoMeeting {
    pg: PgFixture,
    _engine: EngineHandle,
    _stub: Container<GenericImage>,
    store_port: u16,
    livekit: String,
    worker: String,
    containers: Vec<String>,
}

fn video_meeting() -> &'static VideoMeeting {
    static M: OnceLock<VideoMeeting> = OnceLock::new();
    M.get_or_init(|| {
        std::thread::spawn(boot)
            .join()
            .expect("video meeting topology")
    })
}

/// The worker image, built with the repo's own target when it is not there yet (its first build
/// downloads libwebrtc and takes minutes; later ones are cached).
fn ensure_worker_image() {
    let present = Command::new("docker")
        .args(["image", "inspect", WORKER_IMAGE])
        .output()
        .is_ok_and(|o| o.status.success());
    if present {
        return;
    }
    let root = util::examples_dir().join("..");
    let status = Command::new("make")
        .arg("-C")
        .arg(&root)
        .arg("worker-image")
        .status()
        .expect("make worker-image");
    assert!(status.success(), "make worker-image failed");
}

fn start_stub(network: &str) -> Container<GenericImage> {
    let script_dir = util::examples_dir().join("model-stub");
    let container = GenericImage::new("python", "3.12-alpine")
        .with_exposed_port(8000.tcp())
        .with_wait_for(WaitFor::message_on_stdout("listening on 8000"))
        .with_mount(
            Mount::bind_mount(script_dir.to_string_lossy().to_string(), "/stub")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_cmd(["python3", "/stub/model-stub.py", "8000"])
        .with_container_name(format!("videomeet-model-stub-{}", util::short_id()))
        .start()
        .expect("start python:3.12-alpine (docker required)");
    sutra_testkit::reap_on_exit(container.id());
    docker(&[
        "network",
        "connect",
        "--alias",
        "model-stub",
        network,
        container.id(),
    ]);
    container
}

/// `-e` pairs for the LiveKit dev key pair, given to the worker images as references.
fn livekit_env(livekit: &str) -> Vec<String> {
    [
        format!("LIVEKIT_URL=ws://{livekit}:7880"),
        "SUTRA_WORKER_API_KEY=env:LK_KEY".to_string(),
        "SUTRA_WORKER_API_SECRET=env:LK_SECRET".to_string(),
        "LK_KEY=devkey".to_string(),
        "LK_SECRET=secret".to_string(),
    ]
    .into_iter()
    .flat_map(|e| ["-e".to_string(), e])
    .collect()
}

fn logs(container: &str) -> String {
    Command::new("docker")
        .args(["logs", container])
        .output()
        .map(|o| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        })
        .unwrap_or_default()
}

fn wait_for_log(container: &str, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let text = logs(container);
        if text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{container} never logged {needle:?}: {text}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn boot() -> VideoMeeting {
    ensure_worker_image();
    let network = util::network_for(SUITE);
    let _ = Command::new("docker")
        .args(["network", "create", network])
        .output();
    let stub = start_stub(network);
    let pg = engine::start_postgres(SUITE);

    let archives = util::world_readable_temp_dir("sutra-archives");
    let composed = compose::compose_variant(
        &util::examples_dir().join("video-meeting-assistant"),
        "video-stub",
    );
    sutra_loader::package::assemble_dir(
        &composed,
        &archives,
        &sutra_loader::PackageOptions::default(),
    )
    .unwrap_or_else(|e| panic!("package video-meeting-assistant: {e}"));
    util::make_world_readable(&archives);

    let builder = EngineBuilder::new(SUITE, &pg);
    let engine_name = builder.container_name().to_string();
    let store = format!("videomeet-store-{}", util::short_id());
    let livekit = format!("videomeet-livekit-{}", util::short_id());
    let worker = format!("videomeet-worker-{}", util::short_id());

    let store_id = docker(&[
        "run",
        "-d",
        "--name",
        &store,
        "--network",
        network,
        "-p",
        "127.0.0.1::9000",
        "-e",
        &format!("MINIO_ROOT_USER={STORE_ACCESS}"),
        "-e",
        &format!("MINIO_ROOT_PASSWORD={STORE_SECRET}"),
        MINIO,
        "server",
        "/data",
    ]);
    sutra_testkit::reap_on_exit(&store_id);
    let store_port: u16 = docker(&["port", &store, "9000/tcp"])
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.trim().parse().ok())
        .expect("the object store's host port");

    let livekit_id = docker(&[
        "run",
        "-d",
        "--name",
        &livekit,
        "--network",
        network,
        LIVEKIT,
        "--dev",
        "--bind",
        "0.0.0.0",
    ]);
    sutra_testkit::reap_on_exit(&livekit_id);

    let prompts = util::examples_dir().join("video-meeting-assistant/prompts");
    let engine = builder
        .env("SUTRA_AUDIT_SQL", "true")
        .env("SUTRA_VOIP_MEDIA_TOKEN", "env:VOIP_MEDIA_TOKEN")
        .env("SUTRA_VOIP_PROMPTS_DIR", "/prompts")
        .env(
            "SUTRA_VOIP_RECORDING_STORE",
            "s3://meeting-recordings/sessions",
        )
        .env(
            "SUTRA_VOIP_RECORDING_ENDPOINT",
            format!("http://{store}:9000"),
        )
        .env(
            "SUTRA_VOIP_RECORDING_ACCESS_KEY",
            "env:RECORDINGS_ACCESS_KEY",
        )
        .env(
            "SUTRA_VOIP_RECORDING_SECRET_KEY",
            "env:RECORDINGS_SECRET_KEY",
        )
        .env("SUTRA_VOIP_RECORDING_KEK", "env:RECORDINGS_KEK")
        .env("SUTRA_VOIP_RECORDING_CREATE_BUCKET", "true")
        // The same bucket as the engine's object store `recordings`: the flow's model channel
        // reads the segments from it (`media-store: recordings`).
        .env("SUTRA_OBJECT_STORES", "recordings")
        .env(
            "SUTRA_OBJECT_STORES_RECORDINGS_URI",
            "s3://meeting-recordings/sessions",
        )
        .env(
            "SUTRA_OBJECT_STORES_RECORDINGS_ENDPOINT",
            format!("http://{store}:9000"),
        )
        .env(
            "SUTRA_OBJECT_STORES_RECORDINGS_ACCESS_KEY",
            "env:RECORDINGS_ACCESS_KEY",
        )
        .env(
            "SUTRA_OBJECT_STORES_RECORDINGS_SECRET_KEY",
            "env:RECORDINGS_SECRET_KEY",
        )
        .env("SUTRA_OBJECT_STORES_RECORDINGS_KEK", "env:RECORDINGS_KEK")
        .env("RECORDINGS_ACCESS_KEY", STORE_ACCESS)
        .env("RECORDINGS_SECRET_KEY", STORE_SECRET)
        .env("RECORDINGS_KEK", KEK)
        .env("VOIP_MEDIA_TOKEN", MEDIA_TOKEN)
        .env(
            "MEETING_DB_URL",
            format!("postgresql://{}:5432/postgres", pg.container_name),
        )
        .env("MEETING_DB_USER", "postgres")
        .env("MEETING_DB_PASSWORD", "postgres")
        .env("LLM_API_KEY", "stub-not-a-real-key")
        .mount_read_only(&prompts, "/prompts/video-meeting-assistant")
        .start(&archives);

    // The bot: joins the room and waits; one session, then it exits.
    let mut args: Vec<String> = ["run", "-d", "--name", &worker, "--network", network]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(livekit_env(&livekit));
    for env in [
        format!("SUTRA_WORKER_ROOM={ROOM}"),
        "SUTRA_WORKER_MEDIA_TOKEN=env:MT".to_string(),
        format!("MT={MEDIA_TOKEN}"),
        format!("SUTRA_WORKER_ENGINE_URL=ws://{engine_name}:8080/voip/stream"),
        "SUTRA_WORKER_CALLED=+15550190000".to_string(),
        "SUTRA_WORKER_ONCE=true".to_string(),
        "RUST_LOG=info".to_string(),
    ] {
        args.push("-e".to_string());
        args.push(env);
    }
    args.push(WORKER_IMAGE.to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let worker_id = docker(&refs);
    sutra_testkit::reap_on_exit(&worker_id);
    wait_for_log(&worker, "joined the room", Duration::from_secs(60));

    let containers = vec![
        engine.container.id().to_string(),
        livekit.clone(),
        worker.clone(),
        stub.id().to_string(),
    ];
    VideoMeeting {
        pg,
        _engine: engine,
        _stub: stub,
        store_port,
        livekit,
        worker,
        containers,
    }
}

/// Start a scripted participant; answers its container name.
fn participant(
    m: &VideoMeeting,
    network: &str,
    identity: &str,
    name: &str,
    role: &str,
    talk: &str,
    tone: &str,
) -> String {
    let container = format!("videomeet-{identity}-{}", util::short_id());
    let livekit = &m.livekit;
    let mut args: Vec<String> = ["run", "-d", "--name", &container, "--network", network]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(livekit_env(livekit));
    args.extend(
        [
            "--entrypoint",
            "sutra-webrtc-participant",
            WORKER_IMAGE,
            "--room",
            ROOM,
            "--identity",
            identity,
            "--name",
            name,
            "--role",
            role,
            "--talk",
            talk,
            "--duration",
            &DURATION_S.to_string(),
            "--tone",
            tone,
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let id = docker(&refs);
    sutra_testkit::reap_on_exit(&id);
    container
}

// ---- helpers --------------------------------------------------------------------------------

fn sql(m: &VideoMeeting, query: &str) -> String {
    let out = Command::new("docker")
        .args([
            "exec",
            m.pg.container.id(),
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-tAc",
            query,
        ])
        .output()
        .expect("docker exec psql");
    assert!(
        out.status.success(),
        "psql {query}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn store_items(m: &VideoMeeting, store: &str) -> Vec<(String, Value)> {
    if sql(m, "SELECT to_regclass('public.data_store') IS NOT NULL") != "t" {
        return Vec::new();
    }
    sql(
        m,
        &format!(
            "SELECT store_key || E'\\t' || store_value FROM data_store WHERE store_name = '{store}' ORDER BY store_key"
        ),
    )
    .lines()
    .filter_map(|line| {
        let (key, value) = line.split_once('\t')?;
        Some((key.to_string(), serde_json::from_str(value).ok()?))
    })
    .collect()
}

fn dump_logs(m: &VideoMeeting) {
    for container in &m.containers {
        let text = logs(container);
        let tail: Vec<&str> = text.lines().rev().take(150).collect();
        eprintln!(
            "==== {container}\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
}

fn recordings_store(m: &VideoMeeting) -> sutra_objectstore::S3ObjectStore {
    let mut config = sutra_objectstore::S3Config::new(
        sutra_objectstore::StoreLocation::parse("s3://meeting-recordings").unwrap(),
        "us-east-1",
        sutra_objectstore::sigv4::Credentials {
            access_key: STORE_ACCESS.into(),
            secret_key: STORE_SECRET.into(),
            session_token: None,
        },
    );
    config.endpoint = Some(format!("http://127.0.0.1:{}", m.store_port));
    sutra_objectstore::S3ObjectStore::new(config).expect("an S3 client")
}

// ---- the meeting --------------------------------------------------------------------------------

/// One video meeting, end to end: joined, streamed per participant, sampled, recorded per
/// participant (sealed, audited by hash), transcribed per track, cut into segments and analysed
/// by the flow for the external participant only, ended cleanly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_video_meeting_assistant_streams_records_transcribes_and_analyses_per_participant() {
    let m = video_meeting();
    let network = util::network_for(SUITE);
    // Alex (staff) and Sam (external) take turns, twice each.
    let alex = participant(
        m,
        network,
        "alex",
        "Alex",
        "staff",
        "1.0-3.5,8.5-11.0",
        "160",
    );
    let sam = participant(
        m,
        network,
        "sam",
        "Sam",
        "external",
        "4.5-7.5,12.0-14.5",
        "220",
    );

    let deadline = Instant::now() + Duration::from_secs(150);
    let (key, record) = loop {
        if let Some(found) = store_items(m, "meetings")
            .into_iter()
            .find(|(k, _)| k.starts_with("session:"))
        {
            break found;
        }
        if Instant::now() > deadline {
            dump_logs(m);
            for p in [&alex, &sam] {
                eprintln!("==== {p}\n{}", logs(p));
            }
            panic!("no session record within 150 s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let call_id = key.strip_prefix("session:").unwrap().to_string();
    assert!(call_id.starts_with(&format!("{ROOM}-")), "{call_id}");
    assert_eq!(record["status"], "reported", "{record}");
    assert_eq!(record["room"], ROOM);

    // THE SESSION ENDED CLEANLY: the worker closed it when the room emptied, and exited 0.
    assert_eq!(record["endReason"], "stop", "{record}");
    wait_for("the worker exited", Duration::from_secs(30), || {
        let state = docker(&[
            "inspect",
            "-f",
            "{{.State.Status}} {{.State.ExitCode}}",
            &m.worker,
        ]);
        (state == "exited 0").then_some(())
    })
    .await;
    assert!(logs(&m.worker).contains("session ended"));

    // CONSENT: the bot announced recording and analysis in the room.
    assert_eq!(record["consentAnnounced"], true, "{record}");

    // THE PARTICIPANTS: both arrived, each with its declared role; only Sam is segmented and
    // analysed — the flow's decision table says why not for Alex.
    let participants = record["participants"].as_array().expect("participants");
    assert_eq!(participants.len(), 2, "{record}");
    let by_label = |label: &str| {
        participants
            .iter()
            .find(|p| p["label"] == label)
            .unwrap_or_else(|| panic!("participant {label}: {record}"))
    };
    let (p_alex, p_sam) = (by_label("Alex"), by_label("Sam"));
    assert_eq!(p_alex["role"], "staff");
    assert_eq!(p_alex["analysed"], false);
    assert_eq!(p_alex["analysisPolicy"], "staff-not-opted-in");
    assert_eq!(p_alex["segmented"], false);
    assert_eq!(p_alex["segments"], 0);
    assert_eq!(p_sam["role"], "external");
    assert_eq!(p_sam["analysed"], true);
    assert_eq!(p_sam["analysisPolicy"], "analysed");
    assert_eq!(p_sam["segmented"], true);
    for p in [p_alex, p_sam] {
        let talk = p["talkMs"].as_u64().unwrap();
        assert!(
            (3_500..=7_500).contains(&talk),
            "talk time from its own track: {p}"
        );
        // SAMPLING: 1 fps over ~16 s, plus frames at turn boundaries.
        let regular = p["frames"]["regular"].as_u64().unwrap();
        assert!((11..=18).contains(&regular), "frames at 1 fps: {p}");
        assert!(p["frames"]["requested"].as_u64().unwrap() >= 1, "{p}");
    }
    let roster = record["roster"].as_array().expect("roster");
    assert!(
        roster.iter().filter(|r| r["event"] == "joined").count() >= 2,
        "{roster:?}"
    );

    // THE TRANSCRIPT: attributed by track — each participant's two stretches, from its own
    // transcription session (each session answers LINES in order).
    let rows: Vec<Value> = store_items(m, "transcripts")
        .into_iter()
        .filter(|(k, _)| k.starts_with(&format!("{call_id}:")))
        .map(|(_, v)| v)
        .collect();
    for (label, participant) in [("Alex", p_alex), ("Sam", p_sam)] {
        let mut mine: Vec<&Value> = rows.iter().filter(|r| r["speaker"] == label).collect();
        mine.sort_by_key(|r| r["startMs"].as_u64());
        let texts: Vec<&str> = mine.iter().filter_map(|r| r["text"].as_str()).collect();
        assert_eq!(texts, LINES, "{label}'s transcript: {rows:?}");
        for row in mine {
            assert_eq!(row["speakerSource"], "track");
            assert_eq!(row["participantId"], participant["participantId"]);
        }
    }
    assert_eq!(rows.len(), 4, "{rows:?}");

    // THE ANALYSIS: per segment, Sam only, by the flow. The model read exactly the media the
    // segment referenced (the stub reports what it received), and the flow kept only the fields
    // it asked for (the stub's extra keys are dropped and listed).
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    let store = recordings_store(m);
    let analyses: Vec<Value> = store_items(m, "analysis")
        .into_iter()
        .filter(|(k, _)| k.starts_with(&format!("{call_id}:")))
        .map(|(_, v)| v)
        .collect();
    assert!(
        analyses.len() >= 2,
        "segments of 5 s over the recorded part of ~16 s: {analyses:?}"
    );
    let mut segments: Vec<u64> = analyses
        .iter()
        .filter_map(|a| a["segment"].as_u64())
        .collect();
    segments.sort();
    segments.dedup();
    assert_eq!(segments.len(), analyses.len(), "one row per segment");
    for a in &analyses {
        assert_eq!(a["participantId"], p_sam["participantId"], "only Sam: {a}");
        assert_eq!(a["status"], "complete", "{a}");
        assert_eq!(a["analysed"], true, "{a}");
        assert_eq!(a["advisory"], true);
        let frames = a["frames"].as_u64().unwrap();
        assert!(frames >= 1, "{a}");
        assert_eq!(a["seen"]["frames"].as_u64(), Some(frames), "{a}");
        assert_eq!(a["seen"]["audio"], true, "{a}");
        assert!(a["result"]["emotion"]["modalities"].is_object(), "{a}");
        assert!(a["result"].get("framesSeen").is_none(), "{a}");
        let removed = a["removedKeys"].as_array().unwrap();
        assert!(removed.iter().any(|k| k == "framesSeen"), "{a}");
        assert_eq!(a["provider"], "gemini", "{a}");
        // The segment's media: sealed objects in the bucket, named by the row's references.
        let audio = &a["media"]["audio"];
        assert_eq!(audio["kind"], "media", "{a}");
        let key = audio["uri"]
            .as_str()
            .unwrap()
            .strip_prefix("s3://meeting-recordings/")
            .unwrap()
            .to_string();
        assert!(key.contains(&format!("/{call_id}/segments/p")), "{key}");
        let stored = store
            .get(&key)
            .await
            .expect("read the bucket")
            .expect("the segment exists in the bucket");
        assert_eq!(
            audio["sha256"],
            sutra_objectstore::sha256_hex(&stored).as_str()
        );
        let (_, wav) = sutra_objectstore::sealed::open(&kek, &key, &stored)
            .expect("the segment opens with the KEK");
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(
            a["media"]["frames"].as_array().map(Vec::len),
            Some(frames as usize)
        );
    }
    // Alex (staff) was never cut into segments: no segment object of Alex in the bucket.
    let alex_index = p_alex["participantIndex"].as_u64().unwrap();
    let written = store
        .list_objects(&format!(
            "sessions/default/video-meeting-assistant/1.0.0/{call_id}/segments/"
        ))
        .await
        .expect("list the bucket");
    assert!(!written.is_empty());
    assert!(
        written
            .iter()
            .all(|o| !o.key.contains(&format!("/segments/p{alex_index}/"))),
        "{written:?}"
    );
    assert_eq!(
        record["analysis"]["results"].as_u64(),
        Some(analyses.len() as u64)
    );
    assert_eq!(record["video"]["framesRetained"], false);

    // THE RECORDINGS: one per participant, sealed; the audited SHA-256 is the stored object's.
    let recordings = record["recordings"].as_array().expect("recordings");
    assert_eq!(recordings.len(), 2, "{record}");
    for reference in recordings {
        assert_eq!(reference["kind"], "media");
        assert_eq!(reference["consentAnnounced"], true);
        let uri = reference["uri"].as_str().unwrap();
        let key = uri
            .strip_prefix("s3://meeting-recordings/")
            .unwrap_or_else(|| panic!("{uri}"));
        assert!(
            key.starts_with(&format!(
                "sessions/default/video-meeting-assistant/1.0.0/{call_id}/p"
            )),
            "{key}"
        );
        let stored = store
            .get(key)
            .await
            .expect("read the bucket")
            .expect("the recording exists in the bucket");
        let sha256 = sutra_objectstore::sha256_hex(&stored);
        assert_eq!(reference["sha256"], sha256.as_str());
        let audited = sql(
            m,
            &format!("SELECT count(*) FROM audit_event WHERE payload_json LIKE '%{sha256}%'"),
        );
        assert_ne!(audited, "0", "the audit trail carries {sha256}");
        let opened = sutra_transport_voip::recording::open_stream(&kek, key, &stored)
            .expect("opens with the KEK");
        assert!(opened.complete, "the recording verifies to its final frame");
        assert_eq!(reference["complete"], true);
        assert_eq!(&opened.plaintext[..4], b"RIFF");
        let duration = reference["durationMs"].as_u64().unwrap();
        assert!((10_000..=30_000).contains(&duration), "{reference}");
    }

    // THE REPORT: rendered from the record, staff not analysed, the hashes named.
    let report = record["report"].as_str().expect("the rendered report");
    assert!(
        report.contains("not analysed (staff-not-opted-in)"),
        "{report}"
    );
    assert!(
        report.contains(recordings[0]["sha256"].as_str().unwrap()),
        "{report}"
    );
}

// ---- tier 1 -----------------------------------------------------------------------------------

/// Both variants compose over `shared/` and pass package validation.
#[test]
fn video_meeting_assistant_variants_lint_clean() {
    let dir = util::examples_dir().join("video-meeting-assistant");
    for variant in ["video-stub", "video-live"] {
        let report = sutra_loader::lint_dir_with_overlays(
            &dir.join("variants").join(variant),
            &[dir.join("shared")],
        );
        let errors: Vec<_> = report.errors().map(|d| d.to_string()).collect();
        assert!(errors.is_empty(), "{variant}: {errors:#?}");
        let warnings: Vec<_> = report
            .warnings()
            .map(|d| d.to_string())
            .filter(|w| !w.contains("SUTRA.CHANNEL.NO_SCHEMA"))
            .collect();
        assert!(warnings.is_empty(), "{variant}: {warnings:#?}");
    }
}
