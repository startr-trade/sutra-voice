use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sutra_transport_voip::pacing::{frames_of, pace};
use tokio::sync::mpsc;
use tokio::time::Instant;

#[test]
fn frames_are_20_ms_and_the_last_is_padded() {
    let frames = frames_of(&[1i16; 330], 8000, 20);
    assert_eq!(frames.len(), 3);
    assert!(frames.iter().all(|f| f.len() == 160));
    assert_eq!(frames[2][9], 1);
    assert_eq!(frames[2][10], 0);
    assert_eq!(frames_of(&[0i16; 640], 16000, 20).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn playback_is_paced_one_frame_per_20_ms_never_prebuffered() {
    let (tx, mut rx) = mpsc::channel::<usize>(1024);
    let start = Instant::now();
    let player = tokio::spawn(async move { pace((0..50).collect(), 20, &tx, || false).await });
    // After 110 ms only frames 0..=5 may have been sent (the first goes at t=0).
    tokio::time::sleep(Duration::from_millis(110)).await;
    let mut early = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        early.push(frame);
    }
    assert_eq!(early, (0..=5).collect::<Vec<_>>());
    let outcome = player.await.unwrap();
    assert_eq!(outcome.frames_sent, 50);
    assert!(!outcome.interrupted);
    assert_eq!(outcome.played_ms(20), 1000);
    // 50 frames take 49 intervals.
    assert_eq!(start.elapsed(), Duration::from_millis(980));
}

#[tokio::test(start_paused = true)]
async fn barge_in_stops_within_one_frame() {
    let (tx, mut rx) = mpsc::channel::<usize>(1024);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let player = tokio::spawn(async move {
        pace((0..100).collect(), 20, &tx, || flag.load(Ordering::SeqCst)).await
    });
    tokio::time::sleep(Duration::from_millis(210)).await;
    stop.store(true, Ordering::SeqCst);
    let outcome = player.await.unwrap();
    assert!(outcome.interrupted);
    assert_eq!(outcome.frames_sent, 11); // t = 0, 20, …, 200
    let mut received = 0;
    while rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, 11);
}
