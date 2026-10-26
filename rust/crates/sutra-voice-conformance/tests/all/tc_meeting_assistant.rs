//! Conformance: `examples/meeting-assistant` end to end — the bot dials a conference, enters the
//! meeting PIN, announces itself, listens, and reports after the call.
//!
//! One docker network carries the whole call path:
//!
//! | container | role |
//! |---|---|
//! | `sutra-voice-engine:dev` | the engine with the `meeting-stub` package, its consent prompt mounted at `SUTRA_VOIP_PROMPTS_DIR`, recordings going to the object store below |
//! | rustpbx, pinned by digest | the gateway: the trunk the bot dials out on, RWI call control, the PCM media bridge |
//! | SIPp (`deploy/sipp`) | the conference: answers, waits while the PIN and the announcement arrive (capturing them), plays a recorded two-speaker conversation, hangs up |
//! | MinIO (community build, pinned by digest) | the S3-compatible object store the recordings are sealed into |
//! | the examples' stub model server | a Realtime transcription session (WebSocket) and the report job's JSON answer |
//! | PostgreSQL | the engine's database (audit_event, capacity_slot) and the package's stores |
//!
//! WHAT IS PROVEN, from one meeting: the bot joins (the conference answered) and its PIN reaches
//! the conference as RFC 2833 telephone events; the consent announcement is played to the
//! conference; the recording is stored in the bucket, sealed, and the SHA-256 the audit trail
//! carries is the stored object's; the transcript rows are written (four segments, speaker
//! "unknown"); a keyword is noted; the report is produced, stored and sent, with the analysis
//! policy applied (the external participant analysed, the internal one not); the trunk slot is
//! released at the hang-up.
//!
//! RTP stays inside the docker network on small explicit ranges: rustpbx 20000-20019, SIPp 6000.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sutra_objectstore::ObjectStore;
use testcontainers::core::{AccessMode, IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::support::compose;
use crate::support::engine::{self, EngineBuilder, EngineHandle, PgFixture};
use crate::support::util;
use crate::tc_voice_assistant::{docker, shared_dir, spoken_passages, wait_for};

/// Fixture names start `meeting-` so every container lands on the one `meeting` suite network.
const SUITE: &str = "meeting";
/// rustpbx 0.5.1 (da5159a), pinned by digest — the same image the example's compose.yaml pins.
const RUSTPBX: &str =
    "ghcr.io/restsend/rustpbx@sha256:c8c9258d27ed11d368fd94e3e9d47b5e0f03973b57106a00ece9fbbd4b5ea8b4";
/// MinIO as built by the community (upstream publishes no images any more) — compose.yaml pins
/// the same digest.
const MINIO: &str =
    "pgsty/minio@sha256:b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372";
const SIPP_IMAGE: &str = "sutra-meeting-sipp:dev";
const API_KEY: &str = "meeting-e2e-key";
const RWI_TOKEN: &str = "dev-only-rwi-token";
const MEDIA_TOKEN: &str = "dev-only-media-token";
const STORE_ACCESS: &str = "meeting-e2e-access";
const STORE_SECRET: &str = "meeting-e2e-secret-key";
const KEK: &str = "meeting-e2e-recordings-kek";
const PIN: &str = "4321";
/// The stub's transcript lines, in order (examples/model-stub, MEETING_LINES).
const LINES: [&str; 4] = [
    "Good morning, everyone. Let's go through the launch plan for the new booking page.",
    "Thanks. The design is ready; the open question is the release date.",
    "Then we agree to ship on the fifteenth. Action item: Priya books the accessibility review.",
    "Agreed. I will send the summary to the client by Friday.",
];

struct Meeting {
    pg: PgFixture,
    engine: EngineHandle,
    _stub: Container<GenericImage>,
    stub_port: u16,
    store_port: u16,
    /// The conference's packet capture (`conference.pcap`).
    captures: PathBuf,
    /// The containers whose logs explain a failure.
    containers: Vec<String>,
}

fn meeting() -> &'static Meeting {
    static M: OnceLock<Meeting> = OnceLock::new();
    M.get_or_init(|| std::thread::spawn(boot).join().expect("meeting topology"))
}

fn start_stub(network: &str) -> (Container<GenericImage>, u16) {
    let script_dir = util::examples_dir().join("model-stub");
    let container = GenericImage::new("python", "3.12-alpine")
        .with_exposed_port(8000.tcp())
        .with_wait_for(WaitFor::message_on_stdout("listening on 8000"))
        .with_mount(
            Mount::bind_mount(script_dir.to_string_lossy().to_string(), "/stub")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_cmd(["python3", "/stub/model-stub.py", "8000"])
        .with_container_name(format!("meeting-model-stub-{}", util::short_id()))
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
    let port = container.get_host_port_ipv4(8000).expect("mapped 8000");
    (container, port)
}

fn boot() -> Meeting {
    let context = util::examples_dir().join("meeting-assistant/deploy/sipp");
    docker(&["build", "-q", "-t", SIPP_IMAGE, &context.to_string_lossy()]);
    let network = util::network_for(SUITE);
    let _ = Command::new("docker")
        .args(["network", "create", network])
        .output();
    let (stub, stub_port) = start_stub(network);
    let pg = engine::start_postgres("meeting");

    let archives = util::world_readable_temp_dir("sutra-archives");
    let composed = compose::compose_variant(
        &util::examples_dir().join("meeting-assistant"),
        "meeting-stub",
    );
    sutra_loader::package::assemble_dir(
        &composed,
        &archives,
        &sutra_loader::PackageOptions::default(),
    )
    .unwrap_or_else(|e| panic!("package meeting-assistant: {e}"));
    util::make_world_readable(&archives);

    let builder = EngineBuilder::new("meeting", &pg);
    let engine_name = builder.container_name().to_string();
    let pbx = format!("meeting-pbx-{}", util::short_id());
    let conference = format!("meeting-conference-{}", util::short_id());
    let store = format!("meeting-store-{}", util::short_id());
    let captures = shared_dir("sutra-meeting-captures");

    // The object store, reachable from the engine by name and from this test on a host port.
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

    // The example's own rustpbx configuration, addressed to this topology's container names.
    let template = std::fs::read_to_string(
        util::examples_dir().join("meeting-assistant/deploy/rustpbx/config.toml"),
    )
    .expect("rustpbx config");
    let config = template.replace("sip:conference:5060", &format!("sip:{conference}:5060"));
    let config_dir = util::world_readable_temp_dir("sutra-meeting-pbx");
    std::fs::write(config_dir.join("config.toml"), config).expect("write rustpbx config");
    util::make_world_readable(&config_dir);
    let pbx_id = docker(&[
        "run",
        "-d",
        "--name",
        &pbx,
        "--network",
        network,
        "-v",
        &format!(
            "{}:/app/config.toml:ro",
            config_dir.join("config.toml").display()
        ),
        RUSTPBX,
        "--conf",
        "/app/config.toml",
    ]);
    sutra_testkit::reap_on_exit(&pbx_id);

    // The conference: answers, captures what it receives (the PIN, the announcement), plays the
    // conversation, hangs up.
    let conference_id = docker(&[
        "run",
        "-d",
        "--name",
        &conference,
        "--network",
        network,
        "-v",
        &format!("{}:/capture", captures.display()),
        "--entrypoint",
        "sh",
        SIPP_IMAGE,
        "-c",
        "tcpdump -U -i eth0 -w /capture/conference.pcap udp portrange 6000-6010 >/dev/null 2>&1 & \
         exec sipp -sf /meeting/conference.xml -p 5060 -mp 6000 -m 10 -nostdin",
    ]);
    sutra_testkit::reap_on_exit(&conference_id);

    let prompts = util::examples_dir().join("meeting-assistant/prompts");
    let engine = builder
        .env("SUTRA_AUDIT_SQL", "true")
        .env("SUTRA_CAPACITY_POOLS", "trunk-main")
        .env("SUTRA_CAPACITY_POOLS_TRUNK_MAIN_SLOTS", "2")
        .env("SUTRA_VOIP_GATEWAY", "rustpbx")
        .env("SUTRA_VOIP_RWI_URL", format!("ws://{pbx}:8080/rwi/v1"))
        .env("SUTRA_VOIP_RWI_TOKEN", "env:PBX_RWI_TOKEN")
        .env("SUTRA_VOIP_MEDIA_TOKEN", "env:VOIP_MEDIA_TOKEN")
        .env(
            "SUTRA_VOIP_PUBLIC_MEDIA_URL",
            format!("ws://{engine_name}:8080"),
        )
        .env("SUTRA_VOIP_TRUNK", "conference")
        .env("SUTRA_VOIP_ORIGINATE_ALLOW", "+1555*")
        .env("SUTRA_VOIP_ORIGINATE_HOME_PREFIX", "+1")
        .env("SUTRA_VOIP_PROMPTS_DIR", "/prompts")
        .env(
            "SUTRA_VOIP_RECORDING_STORE",
            "s3://meeting-recordings/calls",
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
        .env("RECORDINGS_ACCESS_KEY", STORE_ACCESS)
        .env("RECORDINGS_SECRET_KEY", STORE_SECRET)
        .env("RECORDINGS_KEK", KEK)
        .env("PBX_RWI_TOKEN", RWI_TOKEN)
        .env("VOIP_MEDIA_TOKEN", MEDIA_TOKEN)
        .env("MEETING_API_KEY", API_KEY)
        .env(
            "MEETING_DB_URL",
            format!("postgresql://{}:5432/postgres", pg.container_name),
        )
        .env("MEETING_DB_USER", "postgres")
        .env("MEETING_DB_PASSWORD", "postgres")
        .env("LLM_API_KEY", "stub-not-a-real-key")
        .mount_read_only(&prompts, "/prompts/meeting-assistant")
        .start(&archives);

    // The engine's control client is connected once the PBX logged its upgrade.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let logs = Command::new("docker")
            .args(["logs", &pbx])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if logs
            .lines()
            .any(|l| l.contains("/rwi/v1") && l.contains("| 101 |"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the engine never opened its RWI connection to {pbx}: {logs}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    let containers = vec![
        engine.container.id().to_string(),
        pbx,
        conference,
        stub.id().to_string(),
    ];
    Meeting {
        pg,
        engine,
        _stub: stub,
        stub_port,
        store_port,
        captures,
        containers,
    }
}

// ---- helpers --------------------------------------------------------------------------------

fn sql(meeting: &Meeting, query: &str) -> String {
    let out = Command::new("docker")
        .args([
            "exec",
            meeting.pg.container.id(),
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

fn table_exists(meeting: &Meeting, table: &str) -> bool {
    sql(
        meeting,
        &format!("SELECT to_regclass('public.{table}') IS NOT NULL"),
    ) == "t"
}

/// The `meetings` store item `key`, parsed.
fn meetings_item(meeting: &Meeting, key: &str) -> Option<Value> {
    if !table_exists(meeting, "data_store") {
        return None;
    }
    let value = sql(
        meeting,
        &format!(
            "SELECT store_value FROM data_store WHERE store_name = 'meetings' AND store_key = '{key}'"
        ),
    );
    serde_json::from_str(&value).ok()
}

/// The digits a capture received as RFC 2833 telephone events (payload type 101) on SIPp's media
/// port, in order — one per event (an event's packets share its RTP timestamp).
fn telephone_events(pcap: &Path) -> String {
    let data = std::fs::read(pcap).unwrap_or_else(|e| panic!("{}: {e}", pcap.display()));
    assert!(data.len() >= 24, "{} is not a capture", pcap.display());
    let link = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let mut offset = 24;
    let mut digits = String::new();
    let mut last_timestamp = None;
    while offset + 16 <= data.len() {
        let included =
            u32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()) as usize;
        offset += 16;
        let Some(packet) = data.get(offset..offset + included) else {
            break;
        };
        offset += included;
        let ip = match link {
            1 => packet.get(14..),
            113 => packet.get(16..),
            _ => Some(packet),
        };
        let Some(ip) = ip.filter(|ip| ip.len() > 20 && ip[9] == 17) else {
            continue;
        };
        let header = usize::from(ip[0] & 0x0F) * 4;
        let Some(udp) = ip.get(header..).filter(|u| u.len() >= 8 + 12 + 4) else {
            continue;
        };
        let destination = u16::from_be_bytes([udp[2], udp[3]]);
        let rtp = &udp[8..];
        if destination != 6000 || rtp[1] & 0x7F != 101 {
            continue;
        }
        let timestamp = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
        if last_timestamp == Some(timestamp) {
            continue;
        }
        last_timestamp = Some(timestamp);
        digits.push(match rtp[12] {
            d @ 0..=9 => char::from(b'0' + d),
            10 => '*',
            11 => '#',
            12..=15 => char::from(b'A' + rtp[12] - 12),
            _ => '?',
        });
    }
    digits
}

/// Print the topology's logs (a failed run is reaped at exit).
fn dump_logs(meeting: &Meeting) {
    for container in &meeting.containers {
        let out = Command::new("docker")
            .args(["logs", "--tail", "200", container])
            .output();
        if let Ok(out) = out {
            eprintln!(
                "==== {container}\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

async fn post_join(meeting: &Meeting, body: Value) {
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/channels/join",
            meeting.engine.http_port
        ))
        .header("X-Api-Key", API_KEY)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("POST /channels/join");
    assert_eq!(response.status().as_u16(), 202, "the join is accepted");
}

/// The report the flow sent to `report-out` (the stub's sink), once it has been sent.
async fn sent_report(meeting: &Meeting) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let sink: Value = reqwest::get(format!("http://127.0.0.1:{}/sink", meeting.stub_port))
            .await
            .expect("stub sink")
            .json()
            .await
            .expect("sink JSON");
        let found = sink
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|body| serde_json::from_str::<Value>(body.as_str()?).ok())
            .find(|body| body["meetingId"] == "M-1");
        if let Some(found) = found {
            return found;
        }
        assert!(Instant::now() < deadline, "the report was sent: {sink}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn recordings_store(meeting: &Meeting) -> sutra_objectstore::S3ObjectStore {
    let mut config = sutra_objectstore::S3Config::new(
        sutra_objectstore::StoreLocation::parse("s3://meeting-recordings").unwrap(),
        "us-east-1",
        sutra_objectstore::sigv4::Credentials {
            access_key: STORE_ACCESS.into(),
            secret_key: STORE_SECRET.into(),
            session_token: None,
        },
    );
    config.endpoint = Some(format!("http://127.0.0.1:{}", meeting.store_port));
    sutra_objectstore::S3ObjectStore::new(config).expect("an S3 client")
}

// ---- the meeting --------------------------------------------------------------------------------

/// One meeting, end to end: joined with the PIN, announced, recorded (sealed, audited by hash),
/// transcribed, a keyword noted, reported under the analysis policy, the line released.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_meeting_assistant_joins_records_transcribes_and_reports() {
    let meeting = meeting();
    post_join(
        meeting,
        json!({
            "meetingId": "M-1",
            "title": "Launch plan review",
            "dialIn": "+15550142000",
            "pin": PIN,
            "participants": [
                {"name": "Alex Chen", "role": "internal"},
                {"name": "Sam Rivera", "role": "external"}
            ],
            // Asked for, but this deployment does not allow internal analysis.
            "analysis": {"internalOptIn": true}
        }),
    )
    .await;

    // The call holds a trunk line while it is up.
    wait_for(
        "the call holding a trunk line",
        Duration::from_secs(30),
        || {
            (table_exists(meeting, "capacity_slot")
                && sql(meeting, "SELECT count(*) FROM capacity_slot") == "1")
                .then_some(())
        },
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(120);
    let report = loop {
        if let Some(report) = meetings_item(meeting, "report:M-1") {
            break report;
        }
        if Instant::now() > deadline {
            dump_logs(meeting);
            panic!("no report within 120 s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(report["status"], "reported", "{report}");
    assert_eq!(report["callId"], "meeting-M-1");
    assert_eq!(
        report["hangupBy"], "callee",
        "the conference hung up: {report}"
    );

    // The line is back once the call ended.
    wait_for("the line freed", Duration::from_secs(30), || {
        (sql(meeting, "SELECT count(*) FROM capacity_slot") == "0").then_some(())
    })
    .await;

    // JOIN + PIN: the conference received the PIN (and the #) as telephone events.
    let capture = meeting.captures.join("conference.pcap");
    let keys = telephone_events(&capture);
    assert_eq!(keys, format!("{PIN}#"), "the keys the conference heard");

    // CONSENT: the announcement (1.5 s of tone, 100 ms lead-in) reached the conference, before the
    // conversation; the recording says it was played.
    let passages = spoken_passages(&capture);
    assert!(
        passages.iter().any(|n| (68..=82).contains(n)),
        "the conference heard the announcement (passages of loud RTP packets: {passages:?})"
    );
    let recording = &report["recording"];
    assert_eq!(recording["kind"], "media", "{report}");
    assert_eq!(recording["consentAnnounced"], true, "{recording}");
    assert_eq!(recording["format"], "audio/wav");
    assert_eq!(recording["encryption"]["keyId"], "recordings");

    // THE RECORDING: in the bucket, sealed; its SHA-256 is the one the audit trail carries.
    let uri = recording["uri"].as_str().expect("a recording uri");
    let key = uri
        .strip_prefix("s3://meeting-recordings/")
        .unwrap_or_else(|| panic!("{uri}"));
    assert!(
        key.starts_with("calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed"),
        "{key}"
    );
    let stored = recordings_store(meeting)
        .get(key)
        .await
        .expect("read the bucket")
        .expect("the recording exists in the bucket");
    let sha256 = sutra_objectstore::sha256_hex(&stored);
    assert_eq!(recording["sha256"], sha256.as_str());
    let audited = sql(
        meeting,
        "SELECT payload_json FROM audit_event WHERE payload_json LIKE '%\"media\"%' \
         ORDER BY seq DESC LIMIT 1",
    );
    let audited: Value = serde_json::from_str(&audited).expect("an audited media reference");
    assert_eq!(audited["media"][0]["sha256"], sha256.as_str(), "{audited}");
    assert_eq!(audited["media"][0]["uri"], uri);
    assert!(
        !sql(
            meeting,
            &format!("SELECT count(*) FROM audit_event WHERE payload_json LIKE '%{PIN}%'")
        )
        .starts_with('1'),
        "the PIN never reaches the audit trail"
    );
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    // A streamed recording: sealed chunk by chunk, verified to its final frame.
    let opened = sutra_transport_voip::recording::open_stream(&kek, key, &stored)
        .expect("opens with the KEK");
    assert!(opened.complete, "the recording verifies to its final frame");
    assert_eq!(recording["complete"], true);
    assert_eq!(&opened.plaintext[..4], b"RIFF");
    let duration = recording["durationMs"].as_u64().unwrap();
    assert!(
        (18_000..=40_000).contains(&duration),
        "the recording spans the call ({duration} ms)"
    );

    // THE TRANSCRIPT: one row per segment, in order, no speaker labels.
    let rows = sql(
        meeting,
        "SELECT seq || '|' || speaker || '|' || speaker_source || '|' || start_ms || '|' || \
         end_ms || '|' || text FROM transcripts WHERE call_id = 'meeting-M-1' ORDER BY seq",
    );
    let rows: Vec<Vec<&str>> = rows.lines().map(|l| l.splitn(6, '|').collect()).collect();
    assert_eq!(rows.len(), 4, "{rows:?}");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0], (i + 1).to_string());
        assert_eq!((row[1], row[2]), ("unknown", "none"));
        assert!(row[3].parse::<u64>().unwrap() < row[4].parse::<u64>().unwrap());
        assert_eq!(row[5], LINES[i]);
    }
    assert_eq!(report["segments"], 4);
    assert_eq!(report["transcript"]["speakerLabels"], "none");

    // A KEYWORD, noted while the meeting ran.
    let phrases: Vec<&str> = report["keywords"]
        .as_array()
        .expect("keywords")
        .iter()
        .filter_map(|k| k["phrase"].as_str())
        .collect();
    assert!(phrases.contains(&"action item"), "{report}");

    // THE REPORT: the model's minutes, the analysis policy applied, rendered, sent.
    assert_eq!(
        report["decisions"][0],
        "Ship the new booking page on the fifteenth."
    );
    assert_eq!(report["analyzedRoles"], json!(["external"]));
    let speakers = report["speakers"].as_array().expect("speakers");
    let by_role = |role: &str| {
        speakers
            .iter()
            .find(|s| s["role"] == role)
            .unwrap_or_else(|| panic!("a {role} speaker: {report}"))
    };
    assert_eq!(by_role("external")["analyzed"], true);
    assert_eq!(by_role("external")["sentiment"], "positive");
    assert_eq!(by_role("internal")["analyzed"], false);
    assert!(by_role("internal")["sentiment"].is_null(), "{report}");
    assert!(by_role("internal")["emotion"].is_null(), "{report}");
    let text = report["report"].as_str().expect("the rendered report");
    assert!(text.contains("# Launch plan review"), "{text}");
    assert!(
        text.contains(&sha256),
        "the report names the recording's hash"
    );
    assert!(text.contains("not analysed"), "{text}");
    assert!(
        report["acoustic"]["callerTalkMs"].as_u64().unwrap() >= 5_000,
        "{report}"
    );

    let sent = sent_report(meeting).await;
    assert_eq!(sent["recording"]["sha256"], sha256.as_str());
}

// ---- tier 1 -----------------------------------------------------------------------------------

/// Both variants compose over `shared/` and pass package validation, as `sutra lint <variant>
/// --overlay <shared>` would report — the projected transcript store against its DDL, the
/// three processes, the report template, the channels.
#[test]
fn meeting_assistant_variants_lint_clean() {
    let dir = util::examples_dir().join("meeting-assistant");
    for variant in ["meeting-stub", "meeting-live"] {
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
