//! Durable recordings: a long call streams to the store in parts with bounded memory, and a
//! recording interrupted by a crash is completed, verified, amended and delivered by the sweep.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use sutra_objectstore::{MemoryObjectStore, ObjectStore, StoreLocation};
use sutra_transport_voip::audio::wav_unwrap;
use sutra_transport_voip::config::route_set_of;
use sutra_transport_voip::gateway::{GatewayControl, GenericGateway};
use sutra_transport_voip::hub::VoipHub;
use sutra_transport_voip::recording::{open_stream, Recorder, RecordingOwner, MANIFEST_EXTENSION};

use super::support::*;

const KEK: &str = "a test key-encryption secret";
const RATE: u32 = 8000;
/// Small parts, so a test-sized call spans many of them (an S3 store needs 5 MiB).
const PART: usize = 64 * 1024;

fn store() -> MemoryObjectStore {
    MemoryObjectStore::new(StoreLocation::parse("memory://recordings/calls").unwrap())
}

fn recorder(store: &MemoryObjectStore, spool: Option<std::path::PathBuf>) -> Arc<Recorder> {
    Arc::new(
        Recorder::new(
            Arc::new(store.clone()) as Arc<dyn ObjectStore>,
            Some(KEK),
            "rec-key",
            7200,
        )
        .with_part_bytes(PART)
        .with_chunk_ms(100)
        .with_owner("host-1")
        .with_spool_dir(spool),
    )
}

fn owner(call_id: &str) -> RecordingOwner {
    RecordingOwner {
        call_id: call_id.to_string(),
        tenant: "acme".to_string(),
        module_key: "acme/billing/1.0.0".to_string(),
        participant_id: None,
        participant_index: None,
        audit: None,
    }
}

fn spool_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sutra-voip-{name}-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// A 20 ms frame of a ramp, so every sample is recognisable.
fn frame(n: u64) -> Vec<i16> {
    (0..160).map(|i| ((n * 160 + i) % 20_000) as i16).collect()
}

/// Thirty minutes of audio: parts go to the store as the call runs, the audio side never
/// holds more than a chunk, the store side never more than a part (plus a frame).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_call_streams_in_parts_and_memory_stays_bounded() {
    let store = store();
    let spool = spool_dir("long");
    let recorder = recorder(&store, Some(spool.clone()));
    let mut writer = recorder
        .begin("acme/billing/1.0.0", &["long-1"], RATE, owner("long-1"))
        .unwrap();
    let stats = writer.stats();
    let frames: u64 = 30 * 60 * 50;
    let mut peak_pending = 0;
    for n in 0..frames {
        writer.push(&frame(n));
        peak_pending = peak_pending.max(writer.pending_samples());
        if n % 500 == 0 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let chunk = (RATE / 10) as usize;
    assert!(
        peak_pending <= chunk,
        "the audio side held {peak_pending} samples"
    );
    let stored = writer.finish().await.unwrap().unwrap();
    let audio_bytes = frames * 320;
    let parts = stats.parts.load(Ordering::Relaxed);
    assert!(
        parts as usize >= audio_bytes as usize / PART,
        "{parts} parts for {audio_bytes} bytes"
    );
    let peak = stats.peak_buffered.load(Ordering::Relaxed) as usize;
    assert!(
        peak <= PART + 2 * (chunk * 2 + 64),
        "the store side held {peak} bytes"
    );
    assert_eq!(store.pending_upload_count(), 0);
    assert_eq!(recorder.live_count(), 0);

    // The stored object verifies end to end, and is the call.
    let key = "acme/billing/1.0.0/long-1.wav.sealed";
    let object = store.get(key).await.unwrap().unwrap();
    assert_eq!(stored.sha256, sutra_objectstore::sha256_hex(&object));
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    let opened = open_stream(&kek, &format!("calls/{key}"), &object).unwrap();
    assert!(opened.complete);
    let (samples, rate) = wav_unwrap(&opened.plaintext).unwrap();
    assert_eq!(rate, RATE);
    assert_eq!(samples.len() as u64, frames * 160);
    assert_eq!(&samples[..160], frame(0).as_slice());
    assert_eq!(
        &samples[samples.len() - 160..],
        frame(frames - 1).as_slice()
    );
    // The spool is gone; the manifest says complete.
    assert!(std::fs::read_dir(&spool).unwrap().next().is_none());
    let manifest: serde_json::Value = serde_json::from_slice(
        &store
            .get(&format!("{key}{MANIFEST_EXTENSION}"))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["status"], "complete");
    assert_eq!(manifest["sha256"], stored.sha256);
    let _ = std::fs::remove_dir_all(&spool);
}

/// The process dies mid-call: the next run's sweep uploads what the spool still held,
/// completes the upload, verifies it up to its last good frame, amends the manifest
/// (`complete: false`, the captured duration) and delivers the reference on the call's
/// `call-hangup` channel — once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_cut_by_a_crash_is_recovered_amended_and_delivered_once() {
    let store = store();
    let spool = spool_dir("crash");
    let crashed = recorder(&store, Some(spool.clone()));
    let mut writer = crashed
        .begin("acme/billing/1.0.0", &["cut-1"], RATE, owner("cut-1"))
        .unwrap();
    // 12 s of audio: several parts uploaded, the rest still in the spool.
    for n in 0..600 {
        writer.push(&frame(n));
        if n % 50 == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let uploaded = writer.stats().parts.load(Ordering::Relaxed);
    assert!(
        uploaded >= 2,
        "parts were uploaded before the crash ({uploaded})"
    );
    // The crash: the writer goes away without finishing. (Its uploader is let finish what was
    // already queued, so the test knows what reached the spool.)
    drop(writer);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while crashed.live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        store.pending_upload_count(),
        1,
        "the upload is left in progress"
    );

    // The next run of the same host.
    let (intake, mut rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let hub = VoipHub::with_recorder(
        test_config("ws://unused", Some("media-secret")),
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        Some(recorder(&store, Some(spool.clone()))),
    );
    assert_eq!(hub.sweep_recordings().await, 1);
    let hangup = next_on(&mut rx, "hangup").await;
    assert_eq!(hangup.headers["x-call-id"], "cut-1");
    assert_eq!(hangup.headers["x-hangup-reason"], "recording-recovered");
    let body: serde_json::Value = serde_json::from_slice(&hangup.body).unwrap();
    let reference = &body["recording"];
    assert_eq!(reference["kind"], "media", "{body}");
    assert_eq!(reference["complete"], false);
    assert_eq!(reference["recovered"], true);
    let duration = reference["durationMs"].as_u64().unwrap();
    // Everything that reached the spool: the 12 s less at most the chunk still being cut.
    assert!((11_800..=12_000).contains(&duration), "{duration}");

    let key = "acme/billing/1.0.0/cut-1.wav.sealed";
    let object = store.get(key).await.unwrap().expect("completed");
    assert_eq!(reference["sha256"], sutra_objectstore::sha256_hex(&object));
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    let opened = open_stream(&kek, &format!("calls/{key}"), &object).unwrap();
    assert!(!opened.complete, "no final frame: it reads as cut short");
    let (samples, _) = wav_unwrap(&opened.plaintext).unwrap();
    assert_eq!(samples.len() as u64 * 1000 / RATE as u64, duration);
    assert_eq!(&samples[..160], frame(0).as_slice());

    let manifest: serde_json::Value = serde_json::from_slice(
        &store
            .get(&format!("{key}{MANIFEST_EXTENSION}"))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["status"], "recovered");
    assert_eq!(manifest["complete"], false);
    assert_eq!(manifest["durationMs"], duration);
    assert_eq!(manifest["hangupDelivered"], true);
    assert_eq!(store.pending_upload_count(), 0);
    assert!(std::fs::read_dir(&spool).unwrap().next().is_none());

    // A second sweep finds nothing, and delivers nothing.
    assert_eq!(hub.sweep_recordings().await, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err());
    let _ = std::fs::remove_dir_all(&spool);
}

/// A live recording of this run is never swept, and an upload another host is writing is left
/// alone until it is older than the longest recording.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_leaves_live_and_foreign_recordings_alone() {
    let store = store();
    let spool = spool_dir("live");
    let recorder = recorder(&store, Some(spool.clone()));
    let mut writer = recorder
        .begin("acme/billing/1.0.0", &["live-1"], RATE, owner("live-1"))
        .unwrap();
    for n in 0..100 {
        writer.push(&frame(n));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        recorder.sweep().await.is_empty(),
        "this run's own recording"
    );
    let other_host = Arc::new(
        Recorder::new(
            Arc::new(store.clone()) as Arc<dyn ObjectStore>,
            Some(KEK),
            "rec-key",
            7200,
        )
        .with_owner("host-2"),
    );
    assert!(
        other_host.sweep().await.is_empty(),
        "another host's, still young"
    );
    writer.finish().await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&spool);
}
