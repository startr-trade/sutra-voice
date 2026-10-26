//! Tier 2: durable recordings against a real S3-compatible store (MinIO). A recording longer
//! than a part streams as a multipart upload; a recording cut by a crash is recovered — from
//! the spool on the same host, and from the uploaded parts alone on another.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use sutra_transport_voip::audio::wav_unwrap;
use sutra_transport_voip::recording::{
    open_stream, Recorder, RecordingOwner, DEFAULT_PART_BYTES, MANIFEST_EXTENSION, UPLOAD_MARKERS,
};
use sutra_transport_voip::store::{RecordingStore, S3RecordingStore, S3Settings, StoreLocation};

/// MinIO as built by the community, pinned by digest (the engine's meeting suites use the same).
const MINIO: &str =
    "pgsty/minio@sha256:b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372";
const ACCESS: &str = "recordings-test";
const SECRET: &str = "recordings-test-secret";
const KEK: &str = "a test key-encryption secret";
const RATE: u32 = 16_000;

/// A container removed when the test ends, however it ends.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn minio() -> (Container, Arc<S3RecordingStore>) {
    let name = format!(
        "sutra-voip-recording-minio-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "-p",
        "127.0.0.1::9000",
        "-e",
        &format!("MINIO_ROOT_USER={ACCESS}"),
        "-e",
        &format!("MINIO_ROOT_PASSWORD={SECRET}"),
        MINIO,
        "server",
        "/data",
    ]);
    let container = Container(name.clone());
    let port = docker(&["port", &name, "9000/tcp"])
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.trim().parse::<u16>().ok())
        .expect("the store's host port");
    let store = Arc::new(
        S3RecordingStore::new(S3Settings {
            location: StoreLocation::parse("s3://call-recordings/calls").unwrap(),
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            region: "us-east-1".into(),
            credentials: sutra_objectstore::sigv4::Credentials {
                access_key: ACCESS.into(),
                secret_key: SECRET.into(),
                session_token: None,
            },
            timeout: Duration::from_secs(30),
        })
        .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        match store.ensure_bucket().await {
            Ok(()) => break,
            Err(e) if tokio::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("the store never came up: {e}"),
        }
    }
    (container, store)
}

fn recorder(
    store: &Arc<S3RecordingStore>,
    owner: &str,
    spool: Option<std::path::PathBuf>,
) -> Arc<Recorder> {
    Arc::new(
        Recorder::new(
            Arc::clone(store) as Arc<dyn RecordingStore>,
            Some(KEK),
            "rec-key",
            7200,
        )
        .with_owner(owner)
        .with_spool_dir(spool),
    )
}

fn owner(call_id: &str) -> RecordingOwner {
    RecordingOwner {
        call_id: call_id.into(),
        tenant: "acme".into(),
        module_key: "acme/calls/1.0.0".into(),
        participant_id: None,
        participant_index: None,
    }
}

fn frame(n: u64) -> Vec<i16> {
    (0..320).map(|i| ((n * 320 + i) % 30_000) as i16).collect()
}

/// `seconds` of audio, yielding now and then so the uploader keeps up.
async fn speak(writer: &mut sutra_transport_voip::recording::RecordingWriter, seconds: u64) {
    for n in 0..seconds * 50 {
        writer.push(&frame(n));
        if n % 250 == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

fn spool(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sutra-voip-minio-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "docker"]
async fn a_recording_streams_as_a_multipart_upload_and_a_crash_is_recovered() {
    let (_minio, store) = minio().await;
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());

    // 1. Six minutes at 16 kHz (~11.5 MB): three parts.
    let dir = spool("whole");
    let live = recorder(&store, "host-a", Some(dir.clone()));
    let mut writer = live
        .begin("acme/calls/1.0.0", &["whole-1"], RATE, owner("whole-1"))
        .unwrap();
    speak(&mut writer, 360).await;
    let stats = writer.stats();
    let stored = writer.finish().await.unwrap().unwrap();
    assert!(stats.parts.load(std::sync::atomic::Ordering::Relaxed) >= 3);
    assert!(
        stats
            .peak_buffered
            .load(std::sync::atomic::Ordering::Relaxed) as usize
            <= DEFAULT_PART_BYTES + 64 * 1024
    );
    let key = "acme/calls/1.0.0/whole-1.wav.sealed";
    let object = store.get(key).await.unwrap().expect("stored");
    assert_eq!(
        stored.sha256,
        sutra_transport_voip::store::sha256_hex(&object)
    );
    let opened = open_stream(&kek, &format!("calls/{key}"), &object).unwrap();
    assert!(opened.complete);
    let (samples, rate) = wav_unwrap(&opened.plaintext).unwrap();
    assert_eq!((samples.len() as u64, rate), (360 * RATE as u64, RATE));
    assert!(store.list_keys(UPLOAD_MARKERS).await.unwrap().is_empty());

    // 2. A crash after 4 minutes (one part uploaded, the rest spooled); the same host restarts.
    let mut writer = live
        .begin("acme/calls/1.0.0", &["cut-1"], RATE, owner("cut-1"))
        .unwrap();
    speak(&mut writer, 240).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(writer);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while live.live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(store.list_keys(UPLOAD_MARKERS).await.unwrap().len(), 1);
    let restarted = recorder(&store, "host-a", Some(dir.clone()));
    let recovered = restarted.sweep().await;
    assert_eq!(recovered.len(), 1);
    let reference = &recovered[0].reference;
    assert_eq!(
        (
            reference["complete"].clone(),
            reference["recovered"].clone()
        ),
        (false.into(), true.into())
    );
    assert_eq!(
        reference["durationMs"], 240_000,
        "every spooled chunk: {reference}"
    );
    let key = "acme/calls/1.0.0/cut-1.wav.sealed";
    let object = store.get(key).await.unwrap().expect("completed");
    assert_eq!(
        reference["sha256"],
        sutra_transport_voip::store::sha256_hex(&object)
    );
    let opened = open_stream(&kek, &format!("calls/{key}"), &object).unwrap();
    assert!(!opened.complete);
    let manifest: serde_json::Value = serde_json::from_slice(
        &store
            .get(&format!("{key}{MANIFEST_EXTENSION}"))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["status"], "recovered");
    assert_eq!(manifest["durationMs"], 240_000);

    // 3. A crash whose spool is lost (the host is gone): another run of the same owner
    // recovers what the uploaded parts hold.
    let mut writer = live
        .begin("acme/calls/1.0.0", &["lost-1"], RATE, owner("lost-1"))
        .unwrap();
    speak(&mut writer, 240).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(writer);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while live.live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elsewhere = recorder(&store, "host-a", None);
    let recovered = elsewhere.sweep().await;
    assert_eq!(recovered.len(), 1);
    let duration = recovered[0].reference["durationMs"].as_u64().unwrap();
    // One whole part's worth of audio (5 MiB, less the framing): about 163 s.
    assert!((150_000..240_000).contains(&duration), "{duration}");
    assert!(store.list_keys(UPLOAD_MARKERS).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
