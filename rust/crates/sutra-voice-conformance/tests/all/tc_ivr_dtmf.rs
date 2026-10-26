//! Conformance: `examples/ivr-dtmf` — a plain DTMF phone menu, end to end through a real PBX.
//!
//! One docker network carries the whole call path:
//!
//! | container | role |
//! |---|---|
//! | `sutra-voice-engine:dev` | the engine with the `ivr-dtmf` package, its recorded prompts mounted at `SUTRA_VOIP_PROMPTS_DIR` |
//! | rustpbx, pinned by digest | the PBX (network alias `pbx`): the trunk, the menu number's route to its IVR step app (whose provider is the engine), RWI call control, the PCM media bridge, and the registrar the extensions register with |
//! | SIPp × 4 (`deploy/sipp`) | the phones of extensions 200-203: each registers (SIP digest), then answers what the menu transfers |
//! | SIPp, one per call | the caller: dials the menu's number and presses keys as RFC 2833 telephone events, capturing the RTP it hears |
//! | PostgreSQL | the engine's database and the package's three projected stores |
//!
//! WHAT IS PROVEN, one call per test: an open choice is transferred; a known customer reaches a
//! customers-only extension without a PIN; an unknown caller with a valid PIN is transferred and
//! the PIN's note logged; a wrong PIN three times ends in "sorry" and the menu hanging up; an
//! invalid choice replays the menu (a loop back to the same wait) and a valid one then
//! transfers; an expired PIN is refused. Every outcome is read back from `transfer_log` —
//! which never holds a digit — and a transfer is confirmed by the extension's phone answering.
//!
//! The tests share the PBX and the extensions, so they take one lock and run one at a time.
//! RTP stays inside the docker network on small explicit ranges: rustpbx 20000-20019, SIPp 6000.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::support::engine::{self, EngineBuilder, EngineHandle, PgFixture};
use crate::support::util;
use crate::tc_voice_assistant::{docker, shared_dir, spoken_passages, wait_for};

/// Fixture names start `ivr-` so every container lands on the one `ivr` suite network.
const SUITE: &str = "ivr";
/// rustpbx 0.5.1 (da5159a), pinned by digest — the same image the example's compose.yaml pins.
const RUSTPBX: &str =
    "ghcr.io/restsend/rustpbx@sha256:c8c9258d27ed11d368fd94e3e9d47b5e0f03973b57106a00ece9fbbd4b5ea8b4";
const SIPP_IMAGE: &str = "sutra-ivr-sipp:dev";
const RWI_TOKEN: &str = "dev-only-rwi-token";
const MEDIA_TOKEN: &str = "dev-only-media-token";
/// A number in the seeded `customers` store, and one that is not.
const CUSTOMER: &str = "+15557654321";
const STRANGER: &str = "+15550009999";

/// The tests share one PBX and its extensions.
static LINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Ivr {
    pg: PgFixture,
    _engine: EngineHandle,
    /// The extensions' answered-call logs (`ext-<n>.log`) and the callers' packet captures.
    captures: PathBuf,
}

fn ivr() -> &'static Ivr {
    static I: OnceLock<Ivr> = OnceLock::new();
    I.get_or_init(|| std::thread::spawn(boot).join().expect("ivr topology"))
}

fn boot() -> Ivr {
    let context = util::examples_dir().join("ivr-dtmf/deploy/sipp");
    docker(&["build", "-q", "-t", SIPP_IMAGE, &context.to_string_lossy()]);
    let network = util::network_for(SUITE);
    let _ = Command::new("docker")
        .args(["network", "create", network])
        .output();
    let pg = engine::start_postgres("ivr");

    let archives = util::world_readable_temp_dir("sutra-archives");
    sutra_loader::package::assemble_dir(
        &util::examples_dir().join("ivr-dtmf/package"),
        &archives,
        &sutra_loader::PackageOptions::default(),
    )
    .unwrap_or_else(|e| panic!("package ivr-dtmf: {e}"));
    util::make_world_readable(&archives);

    let builder = EngineBuilder::new("ivr", &pg);
    let engine_name = builder.container_name().to_string();
    let pbx = format!("ivr-pbx-{}", util::short_id());
    let captures = shared_dir("sutra-ivr-captures");

    // The example's own rustpbx configuration, the engine addressed by its container name. The
    // PBX keeps the name the package's transfer target uses (`sip:<extension>@pbx`) as an alias.
    let template =
        std::fs::read_to_string(util::examples_dir().join("ivr-dtmf/deploy/rustpbx/config.toml"))
            .expect("rustpbx config");
    let config = template.replace("http://engine:8080", &format!("http://{engine_name}:8080"));
    let config_dir = util::world_readable_temp_dir("sutra-ivr-pbx");
    std::fs::write(config_dir.join("config.toml"), config).expect("write rustpbx config");
    util::make_world_readable(&config_dir);
    let pbx_id = docker(&[
        "run",
        "-d",
        "--name",
        &pbx,
        "--network",
        network,
        "--network-alias",
        "pbx",
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

    // The extensions' phones: each registers with its password, then answers.
    let mut phones = Vec::new();
    for ext in ["200", "201", "202", "203"] {
        let name = format!("ivr-ext-{ext}-{}", util::short_id());
        let id = docker(&[
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            network,
            "-v",
            &format!("{}:/capture", captures.display()),
            "--entrypoint",
            "/ivr/extension.sh",
            SIPP_IMAGE,
            "pbx:5060",
            ext,
            &format!("dev-only-ext-{ext}"),
        ]);
        sutra_testkit::reap_on_exit(&id);
        phones.push((ext, name));
    }

    let prompts = util::examples_dir().join("ivr-dtmf/prompts");
    let engine = builder
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
        .env("SUTRA_VOIP_PROMPTS_DIR", "/prompts")
        .env("PBX_RWI_TOKEN", RWI_TOKEN)
        .env("VOIP_MEDIA_TOKEN", MEDIA_TOKEN)
        .env(
            "IVR_DB_URL",
            format!("postgresql://{}:5432/postgres", pg.container_name),
        )
        .env("IVR_DB_USER", "postgres")
        .env("IVR_DB_PASSWORD", "postgres")
        .mount_read_only(&prompts, "/prompts/ivr-dtmf")
        .start(&archives);

    // Every phone registered, and the engine's control client connected.
    let deadline = Instant::now() + Duration::from_secs(60);
    for (ext, name) in &phones {
        loop {
            let logs = Command::new("docker")
                .args(["logs", name])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            if logs.contains(&format!("extension {ext} registered")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "extension {ext} never registered: {logs}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }
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

    Ivr {
        pg,
        _engine: engine,
        captures,
    }
}

// ---- helpers --------------------------------------------------------------------------------

fn sql(ivr: &Ivr, query: &str) -> String {
    let out = Command::new("docker")
        .args([
            "exec",
            ivr.pg.container.id(),
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

/// One `transfer_log` row.
#[derive(Debug, Clone, PartialEq)]
struct LogRow {
    call_id: String,
    caller: String,
    extension: String,
    outcome: String,
    method: String,
    reason: String,
    note: String,
    menu_attempts: u32,
    pin_attempts: u32,
}

fn log_rows(ivr: &Ivr) -> Vec<LogRow> {
    if sql(ivr, "SELECT to_regclass('public.transfer_log') IS NOT NULL") != "t" {
        return Vec::new();
    }
    sql(
        ivr,
        "SELECT call_id, caller, coalesce(extension, ''), outcome, coalesce(method, ''), \
                coalesce(reason, ''), coalesce(note, ''), menu_attempts, pin_attempts \
         FROM transfer_log ORDER BY logged_at, store_key",
    )
    .lines()
    .filter_map(|line| {
        let f: Vec<&str> = line.split('|').collect();
        (f.len() == 9).then(|| LogRow {
            call_id: f[0].to_string(),
            caller: f[1].to_string(),
            extension: f[2].to_string(),
            outcome: f[3].to_string(),
            method: f[4].to_string(),
            reason: f[5].to_string(),
            note: f[6].to_string(),
            menu_attempts: f[7].parse().unwrap_or(0),
            pin_attempts: f[8].parse().unwrap_or(0),
        })
    })
    .collect()
}

/// How many calls an extension's phone has answered.
fn answered(ivr: &Ivr, ext: &str) -> usize {
    std::fs::read_to_string(ivr.captures.join(format!("ext-{ext}.log")))
        .map(|log| log.lines().filter(|l| *l == "answered").count())
        .unwrap_or(0)
}

/// What one call did.
struct Call {
    /// Its `transfer_log` rows, oldest first.
    rows: Vec<LogRow>,
    /// The RTP the caller heard (`<name>.pcap`).
    capture: PathBuf,
    /// How long the caller's SIPp ran.
    took: Duration,
}

/// One call to the menu from `caller`, pressing `steps` (see `deploy/sipp/make-dtmf.py`), then
/// holding for `hold_secs` unless the PBX hangs up first. Returns once the call's instance has
/// logged its outcome (a transfer, giving up, or the caller hanging up).
async fn place_call(
    ivr: &'static Ivr,
    name: &str,
    caller: &str,
    steps: &[&str],
    hold_secs: u32,
) -> Call {
    let before: Vec<String> = log_rows(ivr).into_iter().map(|r| r.call_id).collect();
    let container = format!("ivr-caller-{}", util::short_id());
    let script = format!(
        "tcpdump -U -i eth0 -w /capture/{name}.pcap udp portrange 6000-6010 >/dev/null 2>&1 & \
         CAP=$!; sleep 1; HOLD_SECS={hold_secs} /ivr/call.sh pbx:5060 {caller} {}; RC=$?; \
         sleep 1; kill $CAP; exit $RC",
        steps.join(" ")
    );
    let network = util::network_for(SUITE).to_string();
    let captures = ivr.captures.clone();
    let started = Instant::now();
    let out = tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args([
                "run",
                "--rm",
                "--name",
                &container,
                "--network",
                &network,
                "-v",
                &format!("{}:/capture", captures.display()),
                "--entrypoint",
                "sh",
                SIPP_IMAGE,
                "-c",
                &script,
            ])
            .output()
            .expect("docker run the caller")
    })
    .await
    .expect("the caller ran");
    let took = started.elapsed();
    assert!(
        out.status.success(),
        "the caller's call failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = wait_for(
        &format!("the outcome of call {name}"),
        Duration::from_secs(60),
        || {
            let rows = log_rows(ivr);
            let call_id = rows
                .iter()
                .find(|r| {
                    !before.contains(&r.call_id)
                        && ["transferred", "gave-up", "hung-up"].contains(&r.outcome.as_str())
                })?
                .call_id
                .clone();
            Some(rows.into_iter().filter(|r| r.call_id == call_id).collect())
        },
    )
    .await;
    // The call's instance has finished and the trunk line is back.
    wait_for("the line freed", Duration::from_secs(30), || {
        (sql(ivr, "SELECT count(*) FROM capacity_slot") == "0").then_some(())
    })
    .await;
    Call {
        rows,
        capture: ivr.captures.join(format!("{name}.pcap")),
        took,
    }
}

fn only_transfer(call: &Call) -> &LogRow {
    assert_eq!(call.rows.len(), 1, "{:#?}", call.rows);
    let row = &call.rows[0];
    assert_eq!(row.outcome, "transferred", "{row:#?}");
    row
}

// ---- the cases --------------------------------------------------------------------------------

/// Key 1 (sales, open): transferred to 201 at once — and 201's phone answers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_open_choice_is_transferred() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let answered_before = answered(ivr, "201");
    let call = place_call(ivr, "open", STRANGER, &["2500ms", "1"], 4).await;
    let row = only_transfer(&call);
    assert_eq!(row.caller, STRANGER);
    assert_eq!(row.extension, "201");
    assert_eq!(row.method, "open");
    assert_eq!((row.menu_attempts, row.pin_attempts), (1, 0));
    wait_for("extension 201 answering", Duration::from_secs(10), || {
        (answered(ivr, "201") > answered_before).then_some(())
    })
    .await;
}

/// Key 2 (support, customers only) from a number in `customers`: no PIN asked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_known_customer_is_transferred_without_a_pin() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let answered_before = answered(ivr, "202");
    let call = place_call(ivr, "customer", CUSTOMER, &["2500ms", "2"], 4).await;
    let row = only_transfer(&call);
    assert_eq!(row.caller, CUSTOMER);
    assert_eq!(row.extension, "202");
    assert_eq!(row.method, "customer");
    assert_eq!(row.pin_attempts, 0, "a customer is never asked for a PIN");
    wait_for("extension 202 answering", Duration::from_secs(10), || {
        (answered(ivr, "202") > answered_before).then_some(())
    })
    .await;
}

/// Key 2 from an unknown number, then the PIN 4321 # (active, no expiry): transferred, and the
/// PIN's note — who it was shared with — logged with the transfer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_unknown_caller_with_a_valid_pin_is_transferred_and_the_note_logged() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let answered_before = answered(ivr, "202");
    let call = place_call(ivr, "pin", STRANGER, &["2500ms", "2", "2000ms", "4321#"], 4).await;
    let row = only_transfer(&call);
    assert_eq!(row.extension, "202");
    assert_eq!(row.method, "pin");
    assert_eq!(row.note, "shared with the partner help desk");
    assert_eq!((row.menu_attempts, row.pin_attempts), (1, 1));
    wait_for("extension 202 answering", Duration::from_secs(10), || {
        (answered(ivr, "202") > answered_before).then_some(())
    })
    .await;
}

/// Key 3 (accounts) from an unknown number, then three wrong PINs: each refusal logged (without
/// the digits), then "sorry" and the MENU hangs up — the caller never has to.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_wrong_pin_three_times_ends_in_a_hang_up() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let answered_before = answered(ivr, "203");
    // Hold for 30 s after the last key: the call ending well before that is the menu hanging up.
    let call = place_call(
        ivr,
        "wrong-pin",
        STRANGER,
        &[
            "2500ms", "3", "2000ms", "0000#", "2500ms", "1111#", "2500ms", "2222#",
        ],
        30,
    )
    .await;
    let outcomes: Vec<(&str, &str, u32)> = call
        .rows
        .iter()
        .map(|r| (r.outcome.as_str(), r.reason.as_str(), r.pin_attempts))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ("pin-refused", "wrong", 1),
            ("pin-refused", "wrong", 2),
            ("pin-refused", "wrong", 3),
            ("gave-up", "pin-attempts-exhausted", 3),
        ],
        "{:#?}",
        call.rows
    );
    assert!(
        call.took < Duration::from_secs(40),
        "the menu hung up (the caller would have held on for 30 s more): {:?}",
        call.took
    );
    assert_eq!(
        answered(ivr, "203"),
        answered_before,
        "nobody was transferred"
    );
    // No row, of any call, holds a digit the caller entered.
    for digits in ["0000", "1111", "2222", "4321"] {
        assert_eq!(
            sql(
                ivr,
                &format!(
                    "SELECT count(*) FROM transfer_log WHERE concat_ws('|', store_key, call_id, \
                     extension, outcome, method, reason, note) LIKE '%{digits}%'"
                )
            ),
            "0",
            "the log never holds the digits ({digits})"
        );
    }
}

/// Key 9 (not on the menu): "not a valid choice", then the menu AGAIN — the same wait, re-entered
/// — and key 1 then transfers. The caller hears the welcome, the refusal, the menu and the
/// hand-over announcement, in that order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_invalid_choice_replays_the_menu_then_a_valid_choice_transfers() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let call = place_call(ivr, "replay", STRANGER, &["2500ms", "9", "2500ms", "1"], 4).await;
    let row = only_transfer(&call);
    assert_eq!(row.extension, "201");
    assert_eq!(row.menu_attempts, 2, "the menu was offered twice: {row:#?}");
    // Each passage is the prompt of the nearest length (prompts/make-prompts.py), in 20 ms
    // packets: welcome 60, invalid 25, menu 40, transferring 30.
    let passages = spoken_passages(&call.capture);
    let prompts = [
        ("welcome", 60),
        ("invalid", 25),
        ("menu", 40),
        ("transferring", 30),
    ];
    let order: Vec<&str> = passages
        .iter()
        .filter_map(|n| {
            prompts
                .iter()
                .min_by_key(|(_, want)| n.abs_diff(*want))
                .filter(|(_, want)| n.abs_diff(*want) <= 4)
                .map(|(name, _)| *name)
        })
        .collect();
    assert_eq!(
        order,
        vec!["welcome", "invalid", "menu", "transferring"],
        "the prompts the caller heard (passages of loud RTP packets: {passages:?})"
    );
}

/// Key 3, then the PIN 1357 # — a real PIN of extension 203, but expired: refused as expired.
/// The caller then hangs up while asked again, and that is logged too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "docker"]
async fn tc_ivr_expired_pin_is_refused() {
    let _line = LINE.lock().await;
    let ivr = ivr();
    let answered_before = answered(ivr, "203");
    let call = place_call(
        ivr,
        "expired",
        STRANGER,
        &["2500ms", "3", "2000ms", "1357#"],
        3,
    )
    .await;
    let outcomes: Vec<(&str, &str)> = call
        .rows
        .iter()
        .map(|r| (r.outcome.as_str(), r.reason.as_str()))
        .collect();
    assert_eq!(
        outcomes,
        vec![("pin-refused", "expired"), ("hung-up", "caller-hung-up")],
        "{:#?}",
        call.rows
    );
    assert_eq!(call.rows[0].extension, "203");
    assert_eq!(answered(ivr, "203"), answered_before);
}

// ---- tier 1 -----------------------------------------------------------------------------------

/// The package composes and passes package validation, as `sutra lint` would report — its
/// three projected stores against their DDL, the menu table, the BPMN.
#[test]
fn ivr_dtmf_package_lints_clean() {
    let report = sutra_loader::lint_dir(&util::examples_dir().join("ivr-dtmf/package"));
    let errors: Vec<_> = report.errors().map(|d| d.to_string()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    // Schema-less JSON call events are legitimate; anything else would be worth a look.
    let warnings: Vec<_> = report
        .warnings()
        .map(|d| d.to_string())
        .filter(|w| !w.contains("SUTRA.CHANNEL.NO_SCHEMA"))
        .collect();
    assert!(warnings.is_empty(), "{warnings:#?}");
}
