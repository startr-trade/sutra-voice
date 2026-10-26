//! Conformance: `examples/voice-assistant` end to end — real phone calls through a real PBX.
//!
//! One docker network carries the whole call path:
//!
//! | container | role |
//! |---|---|
//! | `sutra-voice-engine:dev` | the engine, with both packages (`inbound`, `reminders`, stub variants) |
//! | rustpbx, pinned by digest | the media gateway: SIP trunk, the DID route to its IVR step app (whose provider is the engine), RWI call control, the PCM media bridge to the engine |
//! | SIPp (`deploy/sipp`, Alpine's package) | the carrier: a caller placing a G.711 call and replaying a recorded utterance as RTP, and the trunk side answering the calls the engine originates |
//! | the examples' stub model server | an audio model that speaks a fixed 800 ms tone and an analysis model that answers JSON |
//! | PostgreSQL | the engine's database and both packages' data stores |
//!
//! WHAT IS PROVEN. An inbound call is greeted and answered — the caller's SIPp receives both
//! spoken answers as RTP, measured from its own packet capture — by ONE instance that loops back
//! to its in-call wait after the answer and leaves the loop on the hang-up (no conversation state
//! passes through the data store); the hang-up frees the trunk slot; the post-call analysis is stored per interaction, and a second call from the same
//! number is decided a repeat contact (revising the first call's first-contact resolution). A
//! reminder call (whose retry is a loop with a counter) is originated, answered by the trunk
//! side, holds one turn and hangs up. With
//! the trunk's outbound share full, a second reminder waits until the first call ends.
//!
//! The trunk is the `trunk-main` capacity pool with 2 lines and 1 reserved for inbound, so
//! outbound can hold at most one line while no inbound call is up. The tests share that trunk
//! and the carrier, so they take one lock and run one at a time.
//!
//! RTP stays inside the docker network on small explicit ranges: rustpbx 20000-20019, SIPp 6000.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use testcontainers::core::{AccessMode, IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::support::compose;
use crate::support::engine::{self, EngineBuilder, EngineHandle, PgFixture};
use crate::support::util;

/// Fixture names start `voice-` so every container lands on the one `voice` suite network.
const SUITE: &str = "voice";
/// rustpbx 0.5.1 (da5159a). The project publishes only a moving `latest` tag, so it is pinned by
/// digest; the example's compose.yaml pins the same one.
const RUSTPBX: &str =
    "ghcr.io/restsend/rustpbx@sha256:c8c9258d27ed11d368fd94e3e9d47b5e0f03973b57106a00ece9fbbd4b5ea8b4";
const SIPP_IMAGE: &str = "sutra-voice-sipp:dev";
const API_KEY: &str = "voice-e2e-key";
const RWI_TOKEN: &str = "dev-only-rwi-token";
const MEDIA_TOKEN: &str = "dev-only-media-token";
const CALLER: &str = "+15557654321";
/// The stub's spoken answer to a short utterance.
const VOICE_ANSWER: &str = "STUB VOICE ANSWER: your appointment is confirmed for Tuesday at ten.";

/// The tests share one trunk and one carrier.
static LINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Voice {
    pg: PgFixture,
    engine: EngineHandle,
    _stub: Container<GenericImage>,
    stub_port: u16,
    pbx: String,
    /// The packet captures of the caller runs and of the carrier (`carrier.pcap`).
    captures: PathBuf,
}

pub(crate) fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("docker {args:?}: {e}"));
    assert!(
        out.status.success(),
        "docker {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A world-writable directory: the capturing containers run as root, the engine as uid 10001.
/// (Shared with the `ivr-dtmf` suite, which drives the same PBX and SIPp.)
pub(crate) fn shared_dir(prefix: &str) -> PathBuf {
    let dir = util::world_readable_temp_dir(prefix);
    let _ = Command::new("chmod").arg("0777").arg(&dir).status();
    dir
}

/// The carrier image, built from the example's own `deploy/sipp` (a cached build when nothing
/// changed).
fn ensure_sipp_image() {
    let context = util::examples_dir().join("voice-assistant/deploy/sipp");
    docker(&["build", "-q", "-t", SIPP_IMAGE, &context.to_string_lossy()]);
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
        .with_container_name(format!("voice-model-stub-{}", util::short_id()))
        .start()
        .expect("start python:3.12-alpine (docker required)");
    sutra_testkit::reap_on_exit(container.id());
    let _ = Command::new("docker")
        .args(["network", "create", network])
        .output();
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

fn voice() -> &'static Voice {
    static V: OnceLock<Voice> = OnceLock::new();
    V.get_or_init(|| std::thread::spawn(boot).join().expect("voice topology"))
}

fn boot() -> Voice {
    ensure_sipp_image();
    let network = util::network_for(SUITE);
    let (stub, stub_port) = start_stub(network);
    let pg = engine::start_postgres("voice");

    // Both packages, each composed exactly as `sutra package <variant> --overlay <shared>`.
    let archives = util::world_readable_temp_dir("sutra-archives");
    for (part, variant) in [("inbound", "inbound-stub"), ("reminders", "reminders-stub")] {
        let composed = compose::compose_variant(
            &util::examples_dir().join("voice-assistant").join(part),
            variant,
        );
        sutra_loader::package::assemble_dir(
            &composed,
            &archives,
            &sutra_loader::PackageOptions::default(),
        )
        .unwrap_or_else(|e| panic!("package voice-assistant {part}: {e}"));
    }
    util::make_world_readable(&archives);

    let builder = EngineBuilder::new("voice", &pg);
    let engine_name = builder.container_name().to_string();
    let pbx = format!("voice-pbx-{}", util::short_id());
    let carrier = format!("voice-carrier-{}", util::short_id());
    let captures = shared_dir("sutra-voice-captures");

    // The example's own rustpbx configuration, addressed to this topology's container names.
    let template = std::fs::read_to_string(
        util::examples_dir().join("voice-assistant/deploy/rustpbx/config.toml"),
    )
    .expect("rustpbx config");
    let config = template
        .replace("http://engine:8080", &format!("http://{engine_name}:8080"))
        .replace("sip:carrier:5060", &format!("sip:{carrier}:5060"));
    let config_dir = util::world_readable_temp_dir("sutra-voice-pbx");
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

    // The trunk side: answers what the engine originates, capturing what it receives.
    let carrier_id = docker(&[
        "run",
        "-d",
        "--name",
        &carrier,
        "--network",
        network,
        "-v",
        &format!("{}:/capture", captures.display()),
        "--entrypoint",
        "sh",
        SIPP_IMAGE,
        "-c",
        "tcpdump -U -i eth0 -w /capture/carrier.pcap udp portrange 6000-6010 >/dev/null 2>&1 & \
         exec sipp -sf /voice/carrier.xml -p 5060 -mp 6000 -m 100 -nostdin",
    ]);
    sutra_testkit::reap_on_exit(&carrier_id);

    let engine = builder
        .env("SUTRA_CAPACITY_POOLS", "trunk-main")
        .env("SUTRA_CAPACITY_POOLS_TRUNK_MAIN_SLOTS", "2")
        .env("SUTRA_CAPACITY_POOLS_TRUNK_MAIN_RESERVE_INBOUND", "1")
        .env("SUTRA_VOIP_GATEWAY", "rustpbx")
        .env("SUTRA_VOIP_RWI_URL", format!("ws://{pbx}:8080/rwi/v1"))
        .env("SUTRA_VOIP_RWI_TOKEN", "env:PBX_RWI_TOKEN")
        .env("SUTRA_VOIP_MEDIA_TOKEN", "env:VOIP_MEDIA_TOKEN")
        .env(
            "SUTRA_VOIP_PUBLIC_MEDIA_URL",
            format!("ws://{engine_name}:8080"),
        )
        .env("SUTRA_VOIP_TRUNK", "carrier")
        .env("PBX_RWI_TOKEN", RWI_TOKEN)
        .env("VOIP_MEDIA_TOKEN", MEDIA_TOKEN)
        .env("VOICE_API_KEY", API_KEY)
        .env(
            "VOICE_DB_URL",
            format!("postgresql://{}:5432/postgres", pg.container_name),
        )
        .env("VOICE_DB_USER", "postgres")
        .env("VOICE_DB_PASSWORD", "postgres")
        .env("LLM_API_KEY", "stub-not-a-real-key")
        .expected_deployments(2)
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

    Voice {
        pg,
        engine,
        _stub: stub,
        stub_port,
        pbx,
        captures,
    }
}

// ---- helpers --------------------------------------------------------------------------------

fn sql(voice: &Voice, query: &str) -> String {
    let out = Command::new("docker")
        .args([
            "exec",
            voice.pg.container.id(),
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

/// The trunk lines in use, per direction.
fn lines_in_use(voice: &Voice) -> (u64, u64) {
    let rows = sql(
        voice,
        "SELECT direction, count(*) FROM capacity_slot WHERE pool = 'trunk-main' GROUP BY direction",
    );
    let mut inbound = 0;
    let mut outbound = 0;
    for row in rows.lines() {
        let (direction, count) = row.split_once('|').unwrap_or_default();
        let count: u64 = count.parse().unwrap_or(0);
        match direction {
            "INBOUND" => inbound = count,
            _ => outbound = count,
        }
    }
    (inbound, outbound)
}

/// The data-store items of `store` whose key starts `prefix`, parsed, by key.
fn store_items(voice: &Voice, store: &str, prefix: &str) -> Vec<(String, Value)> {
    let rows = sql(
        voice,
        &format!(
            "SELECT store_key || E'\\t' || store_value FROM data_store \
             WHERE store_name = '{store}' AND store_key LIKE '{prefix}%' ORDER BY updated_at"
        ),
    );
    rows.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('\t')?;
            Some((key.to_string(), serde_json::from_str(value).ok()?))
        })
        .collect()
}

fn store_has_table(voice: &Voice) -> bool {
    sql(voice, "SELECT to_regclass('public.data_store') IS NOT NULL") == "t"
}

pub(crate) async fn wait_for<T>(
    what: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "{what} within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn stub_count(voice: &Voice, route: &str) -> u64 {
    let stats: Value = reqwest::get(format!("http://127.0.0.1:{}/stats", voice.stub_port))
        .await
        .expect("stub stats")
        .json()
        .await
        .expect("stub stats JSON");
    stats[route].as_u64().unwrap_or(0)
}

/// One inbound call from SIPp to the assistant's number (about 13 s: 3 s for the greeting, the
/// utterance, 9 s listening, hang-up), capturing the RTP it receives into `<name>.pcap`.
fn place_call(voice: &Voice, name: &str) -> PathBuf {
    let caller = format!("voice-caller-{}", util::short_id());
    let script = format!(
        "tcpdump -U -i eth0 -w /capture/{name}.pcap udp portrange 6000-6010 >/dev/null 2>&1 & \
         CAP=$!; sleep 1; \
         /voice/call.sh {}:5060; RC=$?; sleep 1; kill $CAP; exit $RC",
        voice.pbx
    );
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--name",
            &caller,
            "--network",
            util::network_for(SUITE),
            "-v",
            &format!("{}:/capture", voice.captures.display()),
            "--entrypoint",
            "sh",
            SIPP_IMAGE,
            "-c",
            &script,
        ])
        .output()
        .expect("docker run the caller");
    assert!(
        out.status.success(),
        "the caller's call failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    voice.captures.join(format!("{name}.pcap"))
}

/// G.711 µ-law to linear PCM.
fn ulaw_decode(byte: u8) -> i32 {
    let u = !byte;
    let sign = u & 0x80;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;
    let magnitude = ((i32::from(mantissa) << 3) + 0x84) << exponent;
    let sample = magnitude - 0x84;
    if sign != 0 {
        -sample
    } else {
        sample
    }
}

/// The spoken passages a capture received: runs of loud (well above comfort noise) PCMU RTP
/// packets addressed to SIPp's media port, as packet counts. A gap of up to 3 quiet packets
/// stays inside one passage.
pub(crate) fn spoken_passages(pcap: &Path) -> Vec<usize> {
    let data = std::fs::read(pcap).unwrap_or_else(|e| panic!("{}: {e}", pcap.display()));
    assert!(data.len() >= 24, "{} is not a capture", pcap.display());
    let link = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let mut offset = 24;
    let mut loudness = Vec::new();
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
        let Some(udp) = ip.get(header..).filter(|u| u.len() > 8 + 12) else {
            continue;
        };
        let destination = u16::from_be_bytes([udp[2], udp[3]]);
        let rtp = &udp[8..];
        if destination != 6000 || rtp[1] & 0x7F != 0 {
            continue;
        }
        let payload = &rtp[12..];
        let energy: f64 = payload
            .iter()
            .map(|b| f64::from(ulaw_decode(*b)).powi(2))
            .sum::<f64>()
            / payload.len().max(1) as f64;
        loudness.push(energy.sqrt() > 1000.0);
    }
    eprintln!(
        "{}: {} PCMU packets to the media port, {} loud",
        pcap.display(),
        loudness.len(),
        loudness.iter().filter(|l| **l).count()
    );
    let mut passages = Vec::new();
    let (mut run, mut quiet) = (0usize, 0usize);
    for loud in loudness {
        if loud {
            run += 1;
            quiet = 0;
        } else if run > 0 {
            quiet += 1;
            if quiet > 3 {
                passages.push(run);
                run = 0;
                quiet = 0;
            }
        }
    }
    if run > 0 {
        passages.push(run);
    }
    passages
}

async fn post_reminder(voice: &Voice, appointment: &str) {
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/channels/reminder",
            voice.engine.http_port
        ))
        .header("X-Api-Key", API_KEY)
        .json(&json!({ "appointmentId": appointment, "name": "Sam",
                       "phone": "+15550001111", "when": "Tuesday at 10:00" }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("POST /channels/reminder");
    assert_eq!(response.status().as_u16(), 202, "the reminder is accepted");
}

async fn reminder_outcome(voice: &Voice, appointment: &str) -> Value {
    wait_for(
        &format!("the outcome of reminder {appointment}"),
        Duration::from_secs(90),
        || {
            if !store_has_table(voice) {
                return None;
            }
            store_items(voice, "reminders", appointment)
                .into_iter()
                .find(|(key, _)| key == appointment)
                .map(|(_, value)| value)
        },
    )
    .await
}

fn instant(value: &Value) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(
        value.as_str().expect("a timestamp"),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap_or_else(|e| panic!("{value}: {e}"))
}

// ---- inbound ----------------------------------------------------------------------------------

/// A call to the number: greeted, the caller's utterance answered (SIPp receives both, as RTP),
/// the line held for the call and freed at its hang-up, and the interaction recorded with the
/// customer's voice analysis and a first-contact resolution. A second call from the same number
/// within the window is a repeat contact, and revises the first call's first-contact resolution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_voice_inbound_call_is_answered_recorded_and_a_repeat_is_recognised() {
    let _line = LINE.lock().await;
    let voice = voice();
    let spoken_before = stub_count(voice, "chat-voice").await;

    // The line is held while the call is up.
    let call = tokio::task::spawn_blocking(move || place_call(voice, "inbound-1"));
    wait_for(
        "the call holding an inbound trunk line",
        Duration::from_secs(20),
        || (lines_in_use(voice).0 == 1).then_some(()),
    )
    .await;
    let capture = call.await.expect("the caller ran");

    let passages = spoken_passages(&capture);
    assert!(
        passages.iter().filter(|p| **p >= 20).count() >= 2,
        "the caller heard the greeting and the answer (passages of loud RTP packets: {passages:?})"
    );
    assert!(
        stub_count(voice, "chat-voice").await >= spoken_before + 2,
        "the greeting and the answer came from the audio model"
    );

    let first = wait_for(
        "the first call's interaction record",
        Duration::from_secs(60),
        || {
            store_has_table(voice)
                .then(|| store_items(voice, "interactions", "call:"))
                .and_then(|items| items.into_iter().next())
        },
    )
    .await;
    assert_eq!(lines_in_use(voice), (0, 0), "the hang-up released the line");
    let (first_key, record) = first;
    assert_eq!(record["caller"], CALLER, "{record}");
    assert_eq!(record["outcome"], "first-contact-resolution", "{record}");
    assert_eq!(record["repeatContact"], false);
    assert_eq!(record["analyzedParties"], json!(["customer"]));
    assert_eq!(record["analysis"]["customer"]["overall"], "neutral");
    assert_eq!(
        record["audioPreserved"], true,
        "the model heard the audio itself"
    );
    assert_eq!(record["acoustic"]["turns"], 1, "{record}");
    assert!(record["acoustic"]["callerTalkMs"].as_u64().unwrap() >= 1000);
    let turns = record["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 1, "{record}");
    let speech = turns[0]["features"]["durationMs"].as_u64().unwrap();
    assert!(
        (1300..=1700).contains(&speech),
        "the turn is the 1.5 s utterance ({speech} ms)"
    );
    assert_eq!(record["hangupBy"], "caller", "{record}");
    assert!(
        store_items(voice, "interactions", "conversation:").is_empty(),
        "the conversation stays in the call's own instance, not in the store"
    );

    // Again, from the same number.
    let capture = tokio::task::spawn_blocking(move || place_call(voice, "inbound-2"))
        .await
        .expect("the caller ran");
    let passages = spoken_passages(&capture);
    assert!(
        passages.iter().filter(|p| **p >= 20).count() >= 2,
        "the repeat caller heard the greeting and the answer: {passages:?}"
    );
    let second = wait_for(
        "the repeat call's interaction record",
        Duration::from_secs(60),
        || {
            store_items(voice, "interactions", "call:")
                .into_iter()
                .find(|(key, _)| *key != first_key)
        },
    )
    .await
    .1;
    assert_eq!(second["outcome"], "resolved-on-repeat", "{second}");
    assert_eq!(second["repeatContact"], true);
    assert_eq!(second["firstContactResolution"], false);
    assert_eq!(
        second["previousCallId"].as_str(),
        first_key.strip_prefix("call:")
    );
    let revised = store_items(voice, "interactions", &first_key)
        .into_iter()
        .next()
        .expect("the first record")
        .1;
    assert_eq!(
        revised["firstContactResolution"], false,
        "a repeat contact means the first call did not resolve it"
    );
    let index = store_items(voice, "interactions", "caller:")
        .into_iter()
        .next()
        .expect("the caller's contact index")
        .1;
    assert_eq!(index["contacts"], 2);
    assert_eq!(lines_in_use(voice), (0, 0));
}

// ---- outbound ---------------------------------------------------------------------------------

/// A reminder: originated on the pooled channel, answered by the trunk side, one turn (the
/// patient's utterance answered), hung up by the flow; the outcome is stored and the line freed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_voice_reminder_call_is_answered_and_holds_a_turn() {
    let _line = LINE.lock().await;
    let voice = voice();
    post_reminder(voice, "R-1").await;
    let outcome = reminder_outcome(voice, "R-1").await;
    assert_eq!(outcome["outcome"], "answered", "{outcome}");
    assert_eq!(outcome["attempts"], 1);
    assert_eq!(outcome["callId"], "reminder-R-1");
    assert_eq!(
        outcome["patientReply"], VOICE_ANSWER,
        "the patient's answer was heard and answered"
    );
    assert!(instant(&outcome["endedAt"]) > instant(&outcome["answeredAt"]));
    wait_for("the line freed", Duration::from_secs(10), || {
        (lines_in_use(voice) == (0, 0)).then_some(())
    })
    .await;
    let passages = spoken_passages(&voice.captures.join("carrier.pcap"));
    assert!(
        passages.iter().filter(|p| **p >= 20).count() >= 2,
        "the trunk side heard the reminder and the answer: {passages:?}"
    );
}

/// The trunk's outbound share is one line: a second reminder WAITS (its originate held in the
/// outbox, no attempt counted) while the first call is up, and dials when that call ends.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_voice_originate_waits_while_the_trunk_is_full() {
    let _line = LINE.lock().await;
    let voice = voice();
    post_reminder(voice, "W-1").await;
    post_reminder(voice, "W-2").await;
    wait_for("one outbound line in use", Duration::from_secs(30), || {
        (lines_in_use(voice).1 == 1).then_some(())
    })
    .await;
    // Both calls are pending, but the trunk never carries two outbound calls at once.
    let mut both = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        both |= lines_in_use(voice).1 > 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !both,
        "a second outbound call took a line it could not have"
    );
    let first = reminder_outcome(voice, "W-1").await;
    let second = reminder_outcome(voice, "W-2").await;
    assert_eq!(first["outcome"], "answered", "{first}");
    assert_eq!(second["outcome"], "answered", "{second}");
    let (a, b) = if instant(&first["answeredAt"]) < instant(&second["answeredAt"]) {
        (first, second)
    } else {
        (second, first)
    };
    assert!(
        instant(&b["answeredAt"]) > instant(&a["endedAt"]),
        "the waiting reminder was answered only after the first call ended: {a} / {b}"
    );
    assert_eq!(
        b["attempts"], 1,
        "waiting for a line is not a failed attempt"
    );
}

// ---- tier 1: every variant composes and lints clean ---------------------------------------

/// Every committed variant of both voice-assistant packages — the live ones CI cannot call
/// included — composes over its `shared/` layer and passes package validation, as `sutra lint
/// <variant> --overlay <shared>` would report.
#[test]
fn voice_assistant_variants_compose_and_lint_clean() {
    for example in ["voice-assistant/inbound", "voice-assistant/reminders"] {
        let dir = util::examples_dir().join(example);
        let mut variants: Vec<_> = std::fs::read_dir(dir.join("variants"))
            .unwrap_or_else(|e| panic!("{example}/variants: {e}"))
            .flatten()
            .map(|e| e.path())
            .collect();
        variants.sort();
        assert!(!variants.is_empty(), "{example} has no variants");
        for variant in variants {
            let report = sutra_loader::lint_dir_with_overlays(&variant, &[dir.join("shared")]);
            let errors: Vec<_> = report.errors().map(|d| d.to_string()).collect();
            assert!(errors.is_empty(), "{}: {errors:#?}", variant.display());
        }
    }
}
