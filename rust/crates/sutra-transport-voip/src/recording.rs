//! Durable recordings: a call's audio streamed to the object store while the call is up, sealed
//! chunk by chunk, and recoverable after a crash.
//!
//! **Streaming.** A recording is a multipart upload ([`crate::store::RecordingStore`]). The
//! call's audio is cut into chunks (one second of audio by default); each chunk is sealed and
//! appended to a local spool file, and every time the spool holds a part's worth (5 MiB, the
//! smallest part an S3 store accepts) it is uploaded as the next part and the spool emptied.
//! Memory per recording is bounded by one part plus a chunk, whatever the call's length.
//!
//! **The sealed-stream format** (`sutra-sealed-stream-1`):
//!
//! ```text
//! "SUTRA-SEALED-STREAM-1\n"     magic, 22 bytes
//! u32 big-endian                length of the header
//! header                        JSON: {scheme, keyId, wrappedKey (base64), contentType, cipher}
//! frame*                        u32 big-endian length || nonce (12) || AES-256-GCM(chunk) || tag (16)
//! ```
//!
//! One fresh data key per recording, wrapped under the deployment's key-encryption key (the
//! engine's envelope encryption, `sutra_crypto::Kek`). Each frame's AAD binds it to the key id,
//! the object key, its index and whether it is the last one, so frames cannot be reordered,
//! dropped, moved to another object or passed off as the end. Frame 0 is the WAV header; the
//! last frame is marked final. A truncated object therefore verifies up to its last good frame
//! and reads as incomplete ([`open_stream`]). An unsealed recording (`encryption: none`) is a
//! plain streamed WAV file.
//!
//! The WAV header is written before the length is known, with the "unknown length" sizes
//! (`0xFFFFFFFF`) streamed WAV uses; [`open_stream`] and [`fix_wav_sizes`] answer the file with
//! its exact sizes.
//!
//! **Crash recovery.** Each recording has a manifest (`<object key>.json`, in the store and in
//! the spool directory): the call it belongs to, who is writing it, and its state. A sweep
//! ([`Recorder::sweep`], at start-up and periodically) finds recordings no live call is writing —
//! left by an earlier run of this process, or older than the longest recording plus a grace —
//! uploads what the spool still holds, completes the upload, verifies it frame by frame, and
//! amends the manifest (`complete: false`, the captured duration, the stored object's SHA-256).
//! The hub then delivers the recovered reference on the call's `call-hangup` channel, so the
//! process's audit trail gets it too.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sutra_crypto::{Aes256GcmCipher, Kek, PayloadCipher, WrappedDataKey};
use tokio::sync::{mpsc, oneshot};

use crate::audio;
use crate::store::{ObjectStoreError, RecordingStore, UploadedPart};

/// The sealed-stream format's magic prefix.
pub const SEALED_STREAM_MAGIC: &[u8] = b"SUTRA-SEALED-STREAM-1\n";
/// The scheme a sealed recording's reference names.
pub const SEALED_STREAM_SCHEME: &str = "sutra-sealed-stream-1";
/// The content type a sealed recording is stored with.
pub const SEALED_STREAM_CONTENT_TYPE: &str = "application/vnd.sutra.sealed-stream";
/// The extension appended to a sealed recording's key.
pub const SEALED_EXTENSION: &str = ".sealed";
/// The extension of a recording's manifest (appended to the recording's key).
pub const MANIFEST_EXTENSION: &str = ".json";
/// The cipher named in the header.
pub const CIPHER: &str = "aes-256-gcm";
/// The media type recordings are written in.
pub const RECORDING_FORMAT: &str = "audio/wav";
/// The smallest part an S3 store accepts (but the last).
pub const DEFAULT_PART_BYTES: usize = 5 * 1024 * 1024;
/// How much audio one sealed chunk holds, by default.
pub const DEFAULT_CHUNK_MS: u64 = 1000;
/// How often the sweep runs, by default.
pub const DEFAULT_SWEEP_SECONDS: u64 = 600;
/// How long past the longest recording an upload nobody claims is left alone.
pub const STALE_GRACE_SECONDS: u64 = 600;
/// A WAV header's length.
pub const WAV_HEADER_BYTES: usize = 44;
/// The bytes a frame adds to its chunk: length prefix, nonce, tag.
pub const FRAME_OVERHEAD: usize = 4 + 12 + 16;
/// Where the marker of each upload in progress is kept (a copy of its manifest), so a sweep
/// finds interrupted uploads by listing objects.
pub const UPLOAD_MARKERS: &str = "sutra-voip-uploads/";

/// The marker key of the recording at `key`.
pub fn upload_marker(key: &str) -> String {
    format!(
        "{UPLOAD_MARKERS}{}{MANIFEST_EXTENSION}",
        &crate::store::sha256_hex(key.as_bytes())[..32]
    )
}

/// How much of an object a verification reads at a time.
const VERIFY_READ_BYTES: u64 = 1024 * 1024;

fn frame_aad(key_id: &str, object_key: &str, index: u64, last: bool) -> Vec<u8> {
    format!(
        "sutra-sealed-stream\x1f{key_id}\x1f{object_key}\x1f{index}\x1f{}",
        u8::from(last)
    )
    .into_bytes()
}

/// A WAV header for mono PCM16 at `rate` whose length is not known yet.
pub fn wav_streaming_header(rate: u32) -> Vec<u8> {
    let mut header = audio::wav_wrap(&[], rate);
    header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    header[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    header
}

/// Write the exact RIFF and data sizes into a streamed WAV file (a no-op on anything else).
pub fn fix_wav_sizes(wav: &mut [u8]) {
    if wav.len() < WAV_HEADER_BYTES || &wav[0..4] != b"RIFF" || &wav[36..40] != b"data" {
        return;
    }
    let data = (wav.len() - WAV_HEADER_BYTES) as u32;
    wav[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    wav[40..44].copy_from_slice(&data.to_le_bytes());
}

/// Seals a recording's chunks, in order.
pub struct StreamSealer {
    cipher: Aes256GcmCipher,
    key_id: String,
    object_key: String,
    next: u64,
}

impl StreamSealer {
    /// A sealer under a fresh data key wrapped by `kek`, and the stream's opening bytes
    /// (magic, header).
    pub fn new(
        kek: &Kek,
        key_id: &str,
        object_key: &str,
        content_type: &str,
    ) -> Result<(StreamSealer, Vec<u8>), String> {
        let wrapped = kek
            .wrap_random_dek(key_id)
            .map_err(|e| format!("wrapping a data key: {e}"))?;
        let data_key = kek
            .unwrap(&wrapped)
            .map_err(|e| format!("unwrapping the data key: {e}"))?;
        let header = json!({
            "scheme": SEALED_STREAM_SCHEME,
            "keyId": key_id,
            "wrappedKey": base64::engine::general_purpose::STANDARD.encode(wrapped.as_bytes()),
            "contentType": content_type,
            "cipher": CIPHER,
        })
        .to_string();
        let mut opening = Vec::with_capacity(SEALED_STREAM_MAGIC.len() + 4 + header.len());
        opening.extend_from_slice(SEALED_STREAM_MAGIC);
        opening.extend_from_slice(&(header.len() as u32).to_be_bytes());
        opening.extend_from_slice(header.as_bytes());
        Ok((
            StreamSealer {
                cipher: Aes256GcmCipher::new(&data_key),
                key_id: key_id.to_string(),
                object_key: object_key.to_string(),
                next: 0,
            },
            opening,
        ))
    }

    /// The next frame: `chunk` sealed, length-prefixed. `last` marks the end of the stream.
    pub fn seal(&mut self, chunk: &[u8], last: bool) -> Result<Vec<u8>, String> {
        let aad = frame_aad(&self.key_id, &self.object_key, self.next, last);
        let body = self
            .cipher
            .encrypt(chunk, &aad)
            .map_err(|e| e.to_string())?;
        self.next += 1;
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }
}

/// The header of a sealed stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamHeader {
    pub key_id: String,
    pub content_type: String,
}

/// What [`open_stream`] could read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedStream {
    pub header: StreamHeader,
    /// Every verified chunk, in order (a WAV file, with its exact sizes).
    pub plaintext: Vec<u8>,
    /// The final frame verified and nothing follows it.
    pub complete: bool,
    /// How many frames verified.
    pub frames: u64,
}

/// Parse a sealed stream's opening; answers the header, the wrapped key and the opening's length.
fn parse_opening(bytes: &[u8]) -> Result<Option<(StreamHeader, Vec<u8>, usize)>, String> {
    let need = SEALED_STREAM_MAGIC.len() + 4;
    if bytes.len() < need {
        if !SEALED_STREAM_MAGIC.starts_with(&bytes[..bytes.len().min(SEALED_STREAM_MAGIC.len())]) {
            return Err("not a sealed stream (no magic)".to_string());
        }
        return Ok(None);
    }
    if !bytes.starts_with(SEALED_STREAM_MAGIC) {
        return Err("not a sealed stream (no magic)".to_string());
    }
    let len_bytes: [u8; 4] = bytes[SEALED_STREAM_MAGIC.len()..need]
        .try_into()
        .map_err(|_| "truncated header length")?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > 64 * 1024 {
        return Err("sealed stream header is too long".to_string());
    }
    let Some(header_bytes) = bytes.get(need..need + len) else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_slice(header_bytes).map_err(|e| format!("sealed header: {e}"))?;
    if value["cipher"].as_str() != Some(CIPHER)
        || value["scheme"].as_str() != Some(SEALED_STREAM_SCHEME)
    {
        return Err(format!(
            "unknown scheme or cipher {} / {}",
            value["scheme"], value["cipher"]
        ));
    }
    let text = |key: &str| {
        value[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("sealed header has no {key}"))
    };
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(text("wrappedKey")?)
        .map_err(|e| format!("wrapped key: {e}"))?;
    Ok(Some((
        StreamHeader {
            key_id: text("keyId")?,
            content_type: text("contentType")?,
        },
        wrapped,
        need + len,
    )))
}

/// Verifies a sealed stream fed in pieces, keeping only what it has not parsed yet.
pub struct StreamVerifier {
    kek_cipher: Option<(Aes256GcmCipher, StreamHeader)>,
    kek: Option<Arc<Kek>>,
    object_key: String,
    buffer: Vec<u8>,
    next: u64,
    /// The final frame verified.
    pub complete: bool,
    /// A frame failed to verify (or followed the final one): nothing after it counts.
    pub broken: bool,
    /// Plaintext bytes of the verified frames.
    pub plaintext_bytes: u64,
    keep: bool,
    /// The verified plaintext, when kept.
    pub plaintext: Vec<u8>,
}

impl StreamVerifier {
    pub fn new(kek: Arc<Kek>, object_key: &str, keep_plaintext: bool) -> StreamVerifier {
        StreamVerifier {
            kek_cipher: None,
            kek: Some(kek),
            object_key: object_key.to_string(),
            buffer: Vec::new(),
            next: 0,
            complete: false,
            broken: false,
            plaintext_bytes: 0,
            keep: keep_plaintext,
            plaintext: Vec::new(),
        }
    }

    pub fn header(&self) -> Option<&StreamHeader> {
        self.kek_cipher.as_ref().map(|(_, h)| h)
    }

    pub fn frames(&self) -> u64 {
        self.next
    }

    /// Feed the next bytes of the object.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.broken {
            return Ok(());
        }
        self.buffer.extend_from_slice(bytes);
        if self.kek_cipher.is_none() {
            let Some((header, wrapped, used)) = parse_opening(&self.buffer)? else {
                return Ok(());
            };
            let kek = self.kek.as_ref().ok_or("no key-encryption key")?;
            let data_key = kek
                .unwrap(&WrappedDataKey::from_parts(header.key_id.clone(), wrapped))
                .map_err(|e| format!("the data key does not unwrap: {e}"))?;
            self.kek_cipher = Some((Aes256GcmCipher::new(&data_key), header));
            self.buffer.drain(..used);
        }
        let mut offset = 0;
        while self.buffer.len() >= offset + 4 {
            let len = u32::from_be_bytes(
                self.buffer[offset..offset + 4]
                    .try_into()
                    .map_err(|_| "frame length")?,
            ) as usize;
            if self.buffer.len() < offset + 4 + len {
                break;
            }
            if self.complete {
                // Anything after the final frame is not part of the recording.
                self.broken = true;
                self.complete = false;
                break;
            }
            let body = &self.buffer[offset + 4..offset + 4 + len];
            let Some((cipher, header)) = self.kek_cipher.as_ref() else {
                break;
            };
            let mut opened = None;
            for last in [false, true] {
                let aad = frame_aad(&header.key_id, &self.object_key, self.next, last);
                if let Ok(plain) = cipher.decrypt(body, &aad) {
                    opened = Some((plain, last));
                    break;
                }
            }
            let Some((plain, last)) = opened else {
                self.broken = true;
                break;
            };
            self.next += 1;
            self.plaintext_bytes += plain.len() as u64;
            if self.keep {
                self.plaintext.extend_from_slice(&plain);
            }
            self.complete = last;
            offset += 4 + len;
        }
        self.buffer.drain(..offset);
        Ok(())
    }

    /// Whether bytes beyond the last whole frame were left over (a torn frame).
    pub fn has_remainder(&self) -> bool {
        !self.buffer.is_empty()
    }
}

/// Open a sealed recording stored under `object_key`: every frame that verifies, in order.
/// An object cut short, or damaged, opens up to its last good frame with `complete: false`.
pub fn open_stream(kek: &Kek, object_key: &str, sealed: &[u8]) -> Result<OpenedStream, String> {
    let (header, wrapped, used) = parse_opening(sealed)?.ok_or("truncated sealed stream")?;
    let data_key = kek
        .unwrap(&WrappedDataKey::from_parts(header.key_id.clone(), wrapped))
        .map_err(|e| format!("the data key does not unwrap: {e}"))?;
    let mut verifier = StreamVerifier {
        kek_cipher: Some((Aes256GcmCipher::new(&data_key), header.clone())),
        kek: None,
        object_key: object_key.to_string(),
        buffer: Vec::new(),
        next: 0,
        complete: false,
        broken: false,
        plaintext_bytes: 0,
        keep: true,
        plaintext: Vec::new(),
    };
    verifier.feed(&sealed[used..])?;
    let complete = verifier.complete && !verifier.has_remainder() && !verifier.broken;
    let mut plaintext = std::mem::take(&mut verifier.plaintext);
    fix_wav_sizes(&mut plaintext);
    Ok(OpenedStream {
        header,
        plaintext,
        complete,
        frames: verifier.next,
    })
}

// ---- the recorder --------------------------------------------------------------------------------

/// What a recording belongs to (kept in its manifest, so a recovery can report it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingOwner {
    pub call_id: String,
    pub tenant: String,
    pub module_key: String,
    pub participant_id: Option<String>,
    pub participant_index: Option<u16>,
}

/// The recordings sink: the store, the sealing key, where in-progress parts are spooled, and
/// which recordings this process is writing.
pub struct Recorder {
    pub store: Arc<dyn RecordingStore>,
    kek: Option<Arc<Kek>>,
    key_id: String,
    pub max_seconds: u64,
    /// The size a part is uploaded at (the last one is smaller).
    pub part_bytes: usize,
    /// How much audio one chunk holds.
    pub chunk_ms: u64,
    spool_dir: Option<PathBuf>,
    /// Who writes (stable across restarts of the same host): `SUTRA_VOIP_INSTANCE_ID`, else
    /// `HOSTNAME`.
    owner: String,
    /// This run of the process.
    boot: String,
    /// The keys of the recordings this run is writing.
    live: Mutex<HashSet<String>>,
}

/// How a recording ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRecording {
    pub uri: String,
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
}

/// A recording the sweep recovered.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredRecording {
    pub owner: RecordingOwner,
    /// Its media reference (`complete`, `recovered: true`, the captured duration, …).
    pub reference: Value,
    /// Whether the hang-up was delivered before the crash (then no recovered hang-up is).
    pub hangup_delivered: bool,
    /// The recording's key (relative), for amending its manifest after the delivery.
    pub key: String,
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn now_unix() -> u64 {
    time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}

fn safe_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `<module path>/<segments…>.wav` (each part made safe for an object key).
pub fn recording_base_key(module_key: &str, segments: &[&str]) -> String {
    let module_path: Vec<String> = module_key.split('/').map(safe_segment).collect();
    let name: Vec<String> = segments.iter().map(|s| safe_segment(s)).collect();
    format!("{}/{}.wav", module_path.join("/"), name.join("/"))
}

impl Recorder {
    /// Build the sink `settings` describe.
    pub fn from_settings(
        settings: &crate::config::RecordingSettings,
    ) -> Result<Recorder, ObjectStoreError> {
        let memory = settings.store.scheme == "memory";
        let store: Arc<dyn RecordingStore> = if memory {
            Arc::new(crate::store::MemoryRecordingStore::new(
                settings.store.clone(),
            ))
        } else {
            Arc::new(crate::store::S3RecordingStore::new(
                crate::store::S3Settings {
                    location: settings.store.clone(),
                    endpoint: settings.endpoint.clone(),
                    region: settings.region.clone(),
                    credentials: sutra_objectstore::sigv4::Credentials {
                        access_key: settings.access_key.clone().unwrap_or_default(),
                        secret_key: settings.secret_key.clone().unwrap_or_default(),
                        session_token: None,
                    },
                    timeout: Duration::from_secs(60),
                },
            )?)
        };
        let recorder = Recorder::new(
            store,
            settings.kek_secret.as_deref(),
            &settings.key_id,
            settings.max_seconds,
        );
        // A memory store does not outlive the process, so there is nothing to recover into.
        let spool = (!memory)
            .then(|| settings.spool_dir.clone())
            .flatten()
            .map(|dir| dir.join(safe_segment(&recorder.owner)));
        Ok(recorder.with_spool_dir(spool))
    }

    pub fn new(
        store: Arc<dyn RecordingStore>,
        kek_secret: Option<&str>,
        key_id: &str,
        max_seconds: u64,
    ) -> Recorder {
        let owner = std::env::var("SUTRA_VOIP_INSTANCE_ID")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "sutra-voip".to_string());
        Recorder {
            store,
            kek: kek_secret.map(|s| Arc::new(Kek::from_secret(s.as_bytes()))),
            key_id: key_id.to_string(),
            max_seconds,
            part_bytes: DEFAULT_PART_BYTES,
            chunk_ms: DEFAULT_CHUNK_MS,
            spool_dir: None,
            owner,
            boot: uuid::Uuid::new_v4().simple().to_string(),
            live: Mutex::new(HashSet::new()),
        }
    }

    /// Spool in-progress parts under `dir` (created when missing); without one they are held in
    /// memory, and a crash loses up to one part.
    pub fn with_spool_dir(mut self, dir: Option<PathBuf>) -> Recorder {
        self.spool_dir = dir.and_then(|dir| match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(dir),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), "voip: the recording spool directory is unusable, parts are held in memory: {e}");
                None
            }
        });
        self
    }

    /// Upload parts at `bytes` (an S3 store needs at least [`DEFAULT_PART_BYTES`]).
    pub fn with_part_bytes(mut self, bytes: usize) -> Recorder {
        self.part_bytes = bytes.max(1);
        self
    }

    pub fn with_chunk_ms(mut self, ms: u64) -> Recorder {
        self.chunk_ms = ms.max(20);
        self
    }

    /// Who this run is (tests stand in for a restart by building a second recorder with the
    /// same owner).
    pub fn with_owner(mut self, owner: &str) -> Recorder {
        self.owner = owner.to_string();
        self
    }

    pub fn sealed(&self) -> bool {
        self.kek.is_some()
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn spool_dir(&self) -> Option<&Path> {
        self.spool_dir.as_deref()
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// How many recordings this run is writing.
    pub fn live_count(&self) -> usize {
        self.live().len()
    }

    /// Start a recording at `<module path>/<segments…>.wav[.sealed]` of mono PCM16 at `rate`.
    pub fn begin(
        self: &Arc<Self>,
        module_key: &str,
        segments: &[&str],
        rate: u32,
        owner: RecordingOwner,
    ) -> Result<RecordingWriter, String> {
        let base = recording_base_key(module_key, segments);
        let (key, content_type) = match &self.kek {
            Some(_) => (
                format!("{base}{SEALED_EXTENSION}"),
                SEALED_STREAM_CONTENT_TYPE,
            ),
            None => (base, RECORDING_FORMAT),
        };
        let full = self.store.full_key(&key);
        if !crate::store::valid_key(&full) {
            return Err(format!("'{full}' is not an object key"));
        }
        let wav_header = wav_streaming_header(rate);
        let (sealer, opening) = match &self.kek {
            Some(kek) => {
                let (mut sealer, mut opening) =
                    StreamSealer::new(kek, &self.key_id, &full, RECORDING_FORMAT)?;
                opening.extend_from_slice(&sealer.seal(&wav_header, false)?);
                (Some(sealer), opening)
            }
            None => (None, wav_header),
        };
        let chunk_samples = ((u64::from(rate) * self.chunk_ms) / 1000).max(1) as usize;
        let manifest = json!({
            "scheme": if self.kek.is_some() { SEALED_STREAM_SCHEME } else { "wav-stream" },
            "key": key,
            "uri": self.store.uri_of(&key),
            "uploadId": Value::Null,
            "callId": owner.call_id,
            "tenant": owner.tenant,
            "moduleKey": owner.module_key,
            "participantId": owner.participant_id,
            "participantIndex": owner.participant_index,
            "rate": rate,
            "sealed": self.kek.is_some(),
            "keyId": self.kek.as_ref().map(|_| self.key_id.clone()),
            "owner": self.owner,
            "boot": self.boot,
            "startedAt": now_rfc3339(),
            "startedAtUnix": now_unix(),
            "status": "recording",
            "complete": false,
            "nextPart": 1,
            "hangupDelivered": false,
        });
        self.live().insert(key.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let stats = Arc::new(WriterStats::default());
        let spool = self.spool_dir.as_ref().map(|dir| SpoolPaths::of(dir, &key));
        let uploader = Uploader {
            recorder: Arc::clone(self),
            key: key.clone(),
            content_type: content_type.to_string(),
            manifest,
            spool,
            memory: Vec::new(),
            part_len: 0,
            upload_id: None,
            parts: Vec::new(),
            sha: Sha256::new(),
            bytes: 0,
            stats: Arc::clone(&stats),
        };
        tokio::spawn(uploader.run(rx));
        let writer = RecordingWriter {
            rate,
            chunk_samples,
            max_samples: self.max_seconds.saturating_mul(u64::from(rate)),
            sealer,
            pending: Vec::new(),
            pending_start: 0,
            overlay: Vec::new(),
            hold_from: None,
            written: 0,
            truncated: false,
            tx,
            stats,
            uri: self.store.uri_of(&key),
            key,
            key_id: self.kek.as_ref().map(|_| self.key_id.clone()),
            failed: None,
        };
        writer.send(opening)?;
        Ok(writer)
    }
}

/// Counters a writer and its uploader share (tests read them).
#[derive(Default, Debug)]
pub struct WriterStats {
    /// The most bytes the uploader held in memory at once.
    pub peak_buffered: std::sync::atomic::AtomicU64,
    /// Parts uploaded.
    pub parts: std::sync::atomic::AtomicU64,
}

enum UploadCommand {
    Bytes(Vec<u8>),
    Finish(oneshot::Sender<Result<FinishedRecording, String>>),
}

/// The audio side of a recording: takes samples in order (and audio played into the call),
/// cuts and seals chunks, and hands them to the recording's uploader.
pub struct RecordingWriter {
    rate: u32,
    chunk_samples: usize,
    max_samples: u64,
    sealer: Option<StreamSealer>,
    /// Samples not sealed yet, from `pending_start` (in recording samples).
    pending: Vec<i16>,
    pending_start: u64,
    /// Played audio still to be mixed into samples not received yet.
    overlay: Vec<i16>,
    /// A playback started here: nothing from here on is sealed until it has been mixed in.
    hold_from: Option<u64>,
    /// Samples taken so far.
    written: u64,
    /// Audio beyond the limit was dropped.
    pub truncated: bool,
    tx: mpsc::UnboundedSender<UploadCommand>,
    stats: Arc<WriterStats>,
    /// The recording's key (relative to the store's prefix) and URI.
    pub key: String,
    pub uri: String,
    /// The key id it is sealed under (`None`: not sealed).
    pub key_id: Option<String>,
    failed: Option<String>,
}

impl RecordingWriter {
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Samples taken so far (the recording's clock).
    pub fn position(&self) -> u64 {
        self.written
    }

    pub fn duration_ms(&self) -> u64 {
        self.written * 1000 / u64::from(self.rate.max(1))
    }

    pub fn stats(&self) -> Arc<WriterStats> {
        Arc::clone(&self.stats)
    }

    /// Samples held before sealing (memory on the audio side).
    pub fn pending_samples(&self) -> usize {
        self.pending.len() + self.overlay.len()
    }

    fn send(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.tx
            .send(UploadCommand::Bytes(bytes))
            .map_err(|_| "the recording's uploader stopped".to_string())
    }

    /// Take the next samples of the call.
    pub fn push(&mut self, samples: &[i16]) {
        let room = self.max_samples.saturating_sub(self.written) as usize;
        if samples.len() > room {
            self.truncated = true;
        }
        let take = &samples[..samples.len().min(room)];
        if take.is_empty() {
            return;
        }
        let from = self.pending.len();
        self.pending.extend_from_slice(take);
        let mixed = self.overlay.len().min(take.len());
        for (slot, add) in self.pending[from..]
            .iter_mut()
            .zip(self.overlay.drain(..mixed))
        {
            *slot = slot.saturating_add(add);
        }
        self.written += take.len() as u64;
        self.flush(false);
    }

    /// A playback starts at `at` (recording samples): hold sealing from there until it is mixed.
    pub fn hold(&mut self, at: u64) {
        self.hold_from = Some(at.max(self.pending_start));
    }

    /// Mix `samples` (what was played) into the recording from `at`, and release the hold.
    pub fn mix(&mut self, at: u64, samples: &[i16]) {
        let pending_end = self.pending_start + self.pending.len() as u64;
        for (i, sample) in samples.iter().enumerate() {
            let position = at + i as u64;
            if position >= self.max_samples {
                break;
            }
            if position < self.pending_start {
                // Already sealed (no hold was taken): what played there is not in the recording.
                continue;
            }
            if position < pending_end {
                let slot = &mut self.pending[(position - self.pending_start) as usize];
                *slot = slot.saturating_add(*sample);
            } else {
                let index = (position - pending_end) as usize;
                if self.overlay.len() <= index {
                    self.overlay.resize(index + 1, 0);
                }
                self.overlay[index] = self.overlay[index].saturating_add(*sample);
            }
        }
        self.hold_from = None;
        self.flush(false);
    }

    fn flush(&mut self, all: bool) {
        loop {
            let limit = self.hold_from.unwrap_or(u64::MAX).max(self.pending_start);
            let whole = self.pending.len() >= self.chunk_samples
                && self.pending_start + self.chunk_samples as u64 <= limit;
            let take = if whole {
                self.chunk_samples
            } else if all && !self.pending.is_empty() {
                self.pending.len().min(self.chunk_samples)
            } else {
                return;
            };
            let chunk: Vec<i16> = self.pending.drain(..take).collect();
            self.pending_start += take as u64;
            self.emit(&chunk, false);
        }
    }

    fn emit(&mut self, samples: &[i16], last: bool) {
        let bytes = audio::pcm16_to_le(samples);
        let frame = match self.sealer.as_mut() {
            Some(sealer) => match sealer.seal(&bytes, last) {
                Ok(frame) => frame,
                Err(e) => {
                    self.failed.get_or_insert(e);
                    return;
                }
            },
            None if bytes.is_empty() => return,
            None => bytes,
        };
        if let Err(e) = self.send(frame) {
            self.failed.get_or_insert(e);
        }
    }

    /// End the recording: what is left (and any played audio past the end) is sealed, the
    /// final frame written and the upload completed. The receiver answers once it is stored.
    pub fn finish(mut self) -> oneshot::Receiver<Result<FinishedRecording, String>> {
        let room = self.max_samples.saturating_sub(self.written) as usize;
        let tail: Vec<i16> = self.overlay.drain(..).take(room).collect();
        self.written += tail.len() as u64;
        self.pending.extend_from_slice(&tail);
        self.hold_from = None;
        self.flush(true);
        self.emit(&[], true);
        let (done, answer) = oneshot::channel();
        match self.failed.take() {
            Some(e) => {
                let _ = done.send(Err(e));
            }
            None => {
                if let Err(mpsc::error::SendError(UploadCommand::Finish(done))) =
                    self.tx.send(UploadCommand::Finish(done))
                {
                    let _ = done.send(Err("the recording's uploader stopped".to_string()));
                }
            }
        }
        answer
    }
}

/// The spool files of one recording.
#[derive(Debug, Clone)]
struct SpoolPaths {
    data: PathBuf,
    meta: PathBuf,
}

impl SpoolPaths {
    fn of(dir: &Path, key: &str) -> SpoolPaths {
        let name = &crate::store::sha256_hex(key.as_bytes())[..32];
        SpoolPaths {
            data: dir.join(format!("{name}.spool")),
            meta: dir.join(format!("{name}.json")),
        }
    }
}

/// The store side of a recording: spools what the writer sends and uploads it part by part.
struct Uploader {
    recorder: Arc<Recorder>,
    key: String,
    content_type: String,
    manifest: Value,
    spool: Option<SpoolPaths>,
    /// The current part's bytes when nothing is spooled.
    memory: Vec<u8>,
    part_len: usize,
    upload_id: Option<String>,
    parts: Vec<UploadedPart>,
    sha: Sha256,
    bytes: u64,
    stats: Arc<WriterStats>,
}

/// Retry an operation on an unavailable store: four attempts, 500 ms doubling.
async fn retried<T, F, Fut>(what: &str, key: &str, mut op: F) -> Result<T, ObjectStoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ObjectStoreError>>,
{
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(ObjectStoreError::Unavailable(m)) if attempt < 4 => {
                tracing::warn!(key = %key, attempt, "voip: recording {what} failed, retrying: {m}");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
}

impl Uploader {
    fn note_buffered(&self, bytes: usize) {
        self.stats
            .peak_buffered
            .fetch_max(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    async fn write_meta(&self) {
        if let Some(spool) = &self.spool {
            if let Err(e) = tokio::fs::write(&spool.meta, self.manifest.to_string()).await {
                tracing::warn!(key = %self.key, "voip: the recording's spool manifest could not be written: {e}");
            }
        }
    }

    async fn put_manifest(&self) {
        let key = format!("{}{MANIFEST_EXTENSION}", self.key);
        let body = self.manifest.to_string().into_bytes();
        let store = &self.recorder.store;
        if let Err(e) = retried("manifest", &key, || {
            store.put(&key, body.clone(), "application/json")
        })
        .await
        {
            tracing::warn!(key = %key, "voip: the recording manifest could not be stored: {e}");
        }
        let marker = upload_marker(&self.key);
        let done = self.manifest["status"] == "complete";
        let marked = if done {
            store.delete(&marker).await
        } else {
            retried("upload marker", &marker, || {
                store.put(&marker, body.clone(), "application/json")
            })
            .await
            .map(|_| ())
        };
        if let Err(e) = marked {
            tracing::warn!(key = %self.key, "voip: the recording's upload marker could not be updated: {e}");
        }
    }

    async fn ensure_upload(&mut self) -> Result<String, ObjectStoreError> {
        if let Some(id) = &self.upload_id {
            return Ok(id.clone());
        }
        let store = Arc::clone(&self.recorder.store);
        let (key, content_type) = (self.key.clone(), self.content_type.clone());
        let id = retried("upload start", &key, || {
            store.create_upload(&key, &content_type)
        })
        .await?;
        self.upload_id = Some(id.clone());
        self.manifest["uploadId"] = json!(id);
        self.write_meta().await;
        self.put_manifest().await;
        Ok(id)
    }

    async fn append(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        self.bytes += bytes.len() as u64;
        self.part_len += bytes.len();
        match &self.spool {
            Some(spool) => {
                use tokio::io::AsyncWriteExt as _;
                let opened = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&spool.data)
                    .await;
                // Flushed before the handle drops: tokio completes a file write in the
                // background, and a part read or a truncation must see every byte before it.
                let written = match opened {
                    Ok(mut file) => match file.write_all(bytes).await {
                        Ok(()) => file.flush().await,
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                if let Err(e) = written {
                    tracing::warn!(key = %self.key, "voip: the recording spool could not be written, holding the part in memory: {e}");
                    let held = tokio::fs::read(&spool.data).await.unwrap_or_default();
                    self.memory = held;
                    self.memory.extend_from_slice(bytes);
                    self.spool = None;
                }
            }
            None => self.memory.extend_from_slice(bytes),
        }
        self.note_buffered(self.memory.len());
    }

    /// Upload what is held as the next part (`force`: even when smaller than a part).
    async fn upload_held(&mut self, force: bool) -> Result<(), ObjectStoreError> {
        if self.part_len == 0 || (!force && self.part_len < self.recorder.part_bytes) {
            return Ok(());
        }
        let upload_id = self.ensure_upload().await?;
        let body = match &self.spool {
            Some(spool) => tokio::fs::read(&spool.data).await.map_err(|e| {
                ObjectStoreError::Unavailable(format!("reading the recording spool: {e}"))
            })?,
            None => self.memory.clone(),
        };
        self.note_buffered(body.len());
        let number = self.parts.len() as u32 + 1;
        let size = body.len() as u64;
        let store = Arc::clone(&self.recorder.store);
        let key = self.key.clone();
        let etag = retried("part upload", &key, || {
            store.upload_part(&key, &upload_id, number, body.clone())
        })
        .await?;
        self.parts.push(UploadedPart { number, etag, size });
        self.stats
            .parts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The spool is emptied BEFORE the manifest moves on: a crash in between leaves an empty
        // spool and a manifest naming the uploaded part as next, which a recovery overwrites
        // with nothing — never the same bytes uploaded twice.
        match &self.spool {
            Some(spool) => {
                if let Err(e) = tokio::fs::write(&spool.data, b"").await {
                    tracing::warn!(key = %self.key, "voip: the recording spool could not be emptied: {e}");
                }
            }
            None => self.memory = Vec::new(),
        }
        self.part_len = 0;
        self.manifest["nextPart"] = json!(number + 1);
        self.write_meta().await;
        Ok(())
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<UploadCommand>) {
        self.write_meta().await;
        if let Err(e) = self.ensure_upload().await {
            tracing::warn!(key = %self.key, "voip: the recording upload could not start yet: {e}");
        }
        while let Some(command) = rx.recv().await {
            match command {
                UploadCommand::Bytes(bytes) => {
                    self.append(&bytes).await;
                    if let Err(e) = self.upload_held(false).await {
                        tracing::warn!(key = %self.key, "voip: a recording part could not be uploaded yet: {e}");
                    }
                }
                UploadCommand::Finish(done) => {
                    let outcome = self.complete().await;
                    self.recorder.live().remove(&self.key);
                    let _ = done.send(outcome);
                    return;
                }
            }
        }
        // The writer went away without finishing (the call's state was dropped): the upload and
        // its spool stay as a crash would leave them, for the sweep.
        self.recorder.live().remove(&self.key);
    }

    async fn complete(&mut self) -> Result<FinishedRecording, String> {
        let result = async {
            self.upload_held(true).await?;
            let upload_id = self.ensure_upload().await?;
            let store = Arc::clone(&self.recorder.store);
            let (key, parts) = (self.key.clone(), self.parts.clone());
            retried("upload completion", &key, || {
                store.complete_upload(&key, &upload_id, &parts)
            })
            .await
        }
        .await;
        let digest = std::mem::take(&mut self.sha).finalize();
        let sha256: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        match result {
            Ok(()) => {
                self.manifest["status"] = json!("complete");
                self.manifest["complete"] = json!(true);
                self.manifest["sha256"] = json!(sha256);
                self.manifest["bytes"] = json!(self.bytes);
                self.manifest["hangupDelivered"] = json!(true);
                self.put_manifest().await;
                if let Some(spool) = &self.spool {
                    let _ = tokio::fs::remove_file(&spool.data).await;
                    let _ = tokio::fs::remove_file(&spool.meta).await;
                }
                Ok(FinishedRecording {
                    uri: self.recorder.store.uri_of(&self.key),
                    key: self.recorder.store.full_key(&self.key),
                    sha256,
                    bytes: self.bytes,
                })
            }
            Err(e) => {
                // Left for the sweep; the hang-up (carrying the failure) is being delivered, so
                // a recovery must not deliver another.
                self.manifest["status"] = json!("failed");
                self.manifest["hangupDelivered"] = json!(true);
                self.write_meta().await;
                self.put_manifest().await;
                Err(e.to_string())
            }
        }
    }
}

// ---- recovery ------------------------------------------------------------------------------------

impl Recorder {
    /// Find recordings no live call is writing, complete them with what was captured, and amend
    /// their manifests. Answers what was recovered (the hub delivers the hang-ups).
    pub async fn sweep(&self) -> Vec<RecoveredRecording> {
        let mut recovered = Vec::new();
        let mut handled: HashSet<String> = HashSet::new();
        // 1. This host's spools: whatever an earlier run left.
        if let Some(dir) = &self.spool_dir {
            let mut metas = Vec::new();
            if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        metas.push(path);
                    }
                }
            }
            for meta_path in metas {
                let Ok(text) = tokio::fs::read_to_string(&meta_path).await else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(key) = manifest["key"].as_str().map(str::to_string) else {
                    continue;
                };
                if manifest["boot"].as_str() == Some(self.boot.as_str())
                    || self.live().contains(&key)
                {
                    continue;
                }
                handled.insert(key.clone());
                let spool = SpoolPaths::of(dir, &key);
                let data = tokio::fs::read(&spool.data).await.unwrap_or_default();
                match self.recover(manifest, data).await {
                    Ok(Some(r)) => recovered.push(r),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(key = %key, "voip: an interrupted recording could not be recovered yet: {e}");
                        continue;
                    }
                }
                let _ = tokio::fs::remove_file(&spool.data).await;
                let _ = tokio::fs::remove_file(&spool.meta).await;
            }
        }
        // 2. Uploads in the store nobody is writing (another host's, or one whose spool is gone),
        // found through their markers.
        let markers = match self.store.list_keys(UPLOAD_MARKERS).await {
            Ok(markers) => markers,
            Err(e) => {
                tracing::warn!("voip: the recording sweep could not list uploads: {e}");
                return recovered;
            }
        };
        let stale_after = self.max_seconds + STALE_GRACE_SECONDS;
        for marker in markers {
            let Ok(Some(body)) = self.store.get(&marker).await else {
                continue;
            };
            let Ok(mut manifest) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            let Some(key) = manifest["key"].as_str().map(str::to_string) else {
                continue;
            };
            if handled.contains(&key) || self.live().contains(&key) {
                continue;
            }
            let mine = manifest["owner"].as_str() == Some(self.owner.as_str())
                && manifest["boot"].as_str() != Some(self.boot.as_str());
            let age = now_unix().saturating_sub(manifest["startedAtUnix"].as_u64().unwrap_or(0));
            if !mine && age < stale_after {
                continue;
            }
            // The spool (if any) is on another host; recover from the uploaded parts.
            manifest["nextPart"] = Value::Null;
            match self.recover(manifest, Vec::new()).await {
                Ok(Some(r)) => recovered.push(r),
                Ok(None) => {}
                Err(ObjectStoreError::Rejected(e)) => {
                    // The upload is gone (completed or aborted elsewhere): nothing to recover.
                    tracing::warn!(key = %key, "voip: an interrupted recording's upload is gone: {e}");
                    let _ = self.store.delete(&marker).await;
                }
                Err(e) => {
                    tracing::warn!(key = %key, "voip: an interrupted recording could not be recovered yet: {e}")
                }
            }
        }
        recovered
    }

    /// The whole frames of a spool (`from_start`: it begins with the stream's opening).
    fn whole_frames(&self, data: &[u8], from_start: bool) -> usize {
        if self.kek.is_none() {
            // A plain WAV stream: whole samples.
            return data.len() & !1;
        }
        let mut offset = 0;
        if from_start {
            match parse_opening(data) {
                Ok(Some((_, _, used))) => offset = used,
                _ => return 0,
            }
        }
        while data.len() >= offset + 4 {
            let len = u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            if data.len() < offset + 4 + len {
                break;
            }
            offset += 4 + len;
        }
        offset
    }

    async fn recover(
        &self,
        mut manifest: Value,
        spool: Vec<u8>,
    ) -> Result<Option<RecoveredRecording>, ObjectStoreError> {
        let key = manifest["key"].as_str().unwrap_or_default().to_string();
        let store = &self.store;
        let mut upload_id = manifest["uploadId"].as_str().map(str::to_string);
        let mut parts = match &upload_id {
            Some(id) => store.list_parts(&key, id).await?,
            None => Vec::new(),
        };
        let next_part = manifest["nextPart"]
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or(parts.len() as u32 + 1);
        let usable = self.whole_frames(&spool, next_part == 1);
        if usable > 0 {
            let id = match &upload_id {
                Some(id) => id.clone(),
                None => {
                    let content_type = if self.kek.is_some() {
                        SEALED_STREAM_CONTENT_TYPE
                    } else {
                        RECORDING_FORMAT
                    };
                    let id = store.create_upload(&key, content_type).await?;
                    upload_id = Some(id.clone());
                    id
                }
            };
            let body = spool[..usable].to_vec();
            let size = body.len() as u64;
            let etag = store.upload_part(&key, &id, next_part, body).await?;
            parts.retain(|p| p.number != next_part);
            parts.push(UploadedPart {
                number: next_part,
                etag,
                size,
            });
            parts.sort_by_key(|p| p.number);
        }
        let Some(upload_id) = upload_id else {
            return Ok(None);
        };
        // Parts must be contiguous from 1: anything after a gap is unreadable as a stream.
        let mut contiguous = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if part.number != i as u32 + 1 {
                break;
            }
            contiguous.push(part);
        }
        let owner = RecordingOwner {
            call_id: manifest["callId"].as_str().unwrap_or_default().to_string(),
            tenant: manifest["tenant"].as_str().unwrap_or_default().to_string(),
            module_key: manifest["moduleKey"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            participant_id: manifest["participantId"].as_str().map(str::to_string),
            participant_index: manifest["participantIndex"]
                .as_u64()
                .and_then(|i| u16::try_from(i).ok()),
        };
        let hangup_delivered = manifest["hangupDelivered"].as_bool().unwrap_or(false);
        if contiguous.is_empty() {
            store.abort_upload(&key, &upload_id).await?;
            manifest["status"] = json!("empty");
            manifest["complete"] = json!(false);
            manifest["durationMs"] = json!(0);
            let _ = store
                .put(
                    &format!("{key}{MANIFEST_EXTENSION}"),
                    manifest.to_string().into_bytes(),
                    "application/json",
                )
                .await;
            let _ = store.delete(&upload_marker(&key)).await;
            tracing::warn!(target: "sutra.audit", event = "VOIP_RECORDING_RECOVERED", call_id = %owner.call_id, key = %key, captured_ms = 0u64, "voip: an interrupted recording held no audio; its upload was aborted");
            return Ok(None);
        }
        store.complete_upload(&key, &upload_id, &contiguous).await?;
        // Verify the stored object and measure what it holds.
        let (sha256, bytes, audio_bytes, complete) = self.measure(&key).await?;
        let rate = manifest["rate"].as_u64().unwrap_or(8000).max(1);
        let duration_ms = audio_bytes / 2 * 1000 / rate;
        manifest["status"] = json!("recovered");
        manifest["complete"] = json!(complete);
        manifest["durationMs"] = json!(duration_ms);
        manifest["sha256"] = json!(sha256);
        manifest["bytes"] = json!(bytes);
        manifest["recoveredAt"] = json!(now_rfc3339());
        let _ = store
            .put(
                &format!("{key}{MANIFEST_EXTENSION}"),
                manifest.to_string().into_bytes(),
                "application/json",
            )
            .await;
        let _ = store.delete(&upload_marker(&key)).await;
        tracing::warn!(target: "sutra.audit", event = "VOIP_RECORDING_RECOVERED", call_id = %owner.call_id, key = %key, captured_ms = duration_ms, complete, "voip: an interrupted recording was recovered");
        let mut reference = json!({
            "kind": crate::listen::MEDIA_REFERENCE_KIND,
            "uri": store.uri_of(&key),
            "sha256": sha256,
            "bytes": bytes,
            "durationMs": duration_ms,
            "format": RECORDING_FORMAT,
            "rate": rate,
            "channels": 1,
            "encryption": manifest["keyId"].as_str().map(|id| json!({
                "scheme": SEALED_STREAM_SCHEME, "cipher": CIPHER, "keyId": id
            })),
            "complete": complete,
            "recovered": true,
            "callId": owner.call_id,
        });
        if let Some(id) = &owner.participant_id {
            reference["participantId"] = json!(id);
            reference["participantIndex"] = json!(owner.participant_index);
        }
        Ok(Some(RecoveredRecording {
            owner,
            reference,
            hangup_delivered,
            key,
        }))
    }

    /// The stored object's SHA-256, size, verified audio bytes, and whether its end verified.
    async fn measure(&self, key: &str) -> Result<(String, u64, u64, bool), ObjectStoreError> {
        let mut sha = Sha256::new();
        let mut verifier = self
            .kek
            .as_ref()
            .map(|kek| StreamVerifier::new(Arc::clone(kek), &self.store.full_key(key), false));
        let mut offset = 0u64;
        loop {
            let piece = self
                .store
                .get_range(key, offset, VERIFY_READ_BYTES)
                .await?
                .unwrap_or_default();
            if piece.is_empty() {
                break;
            }
            sha.update(&piece);
            if let Some(v) = verifier.as_mut() {
                v.feed(&piece).map_err(ObjectStoreError::Rejected)?;
            }
            offset += piece.len() as u64;
            if (piece.len() as u64) < VERIFY_READ_BYTES {
                break;
            }
        }
        let sha256: String = sha.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let (audio, complete) = match verifier {
            Some(v) => {
                let complete = v.complete && !v.broken && !v.has_remainder();
                (
                    v.plaintext_bytes.saturating_sub(WAV_HEADER_BYTES as u64),
                    complete,
                )
            }
            None => (offset.saturating_sub(WAV_HEADER_BYTES as u64), false),
        };
        Ok((sha256, offset, audio, complete))
    }

    /// Record that a recovered recording's hang-up was delivered.
    pub async fn mark_delivered(&self, key: &str) {
        let manifest_key = format!("{key}{MANIFEST_EXTENSION}");
        if let Ok(Some(body)) = self.store.get(&manifest_key).await {
            if let Ok(mut manifest) = serde_json::from_slice::<Value>(&body) {
                manifest["hangupDelivered"] = json!(true);
                let _ = self
                    .store
                    .put(
                        &manifest_key,
                        manifest.to_string().into_bytes(),
                        "application/json",
                    )
                    .await;
            }
        }
    }
}

/// Group recovered recordings by call (a meeting has one per participant).
pub fn by_call(recovered: Vec<RecoveredRecording>) -> BTreeMap<String, Vec<RecoveredRecording>> {
    let mut out: BTreeMap<String, Vec<RecoveredRecording>> = BTreeMap::new();
    for r in recovered {
        out.entry(r.owner.call_id.clone()).or_default().push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_opens_frame_by_frame_and_a_cut_reads_as_incomplete() {
        let kek = Kek::from_secret(b"a test key-encryption secret");
        let (mut sealer, mut object) =
            StreamSealer::new(&kek, "recordings", "calls/c1.wav.sealed", RECORDING_FORMAT).unwrap();
        object.extend(sealer.seal(&wav_streaming_header(8000), false).unwrap());
        object.extend(sealer.seal(&[1, 0, 2, 0], false).unwrap());
        let cut_at = object.len();
        object.extend(sealer.seal(&[3, 0], false).unwrap());
        object.extend(sealer.seal(&[], true).unwrap());

        let opened = open_stream(&kek, "calls/c1.wav.sealed", &object).unwrap();
        assert!(opened.complete);
        assert_eq!(opened.frames, 4);
        assert_eq!(&opened.plaintext[44..], &[1, 0, 2, 0, 3, 0]);
        let (samples, rate) = audio::wav_unwrap(&opened.plaintext).unwrap();
        assert_eq!((samples, rate), (vec![1, 2, 3], 8000));

        // Cut mid-frame: verifies up to the last whole frame.
        let partial = open_stream(&kek, "calls/c1.wav.sealed", &object[..cut_at + 7]).unwrap();
        assert!(!partial.complete);
        assert_eq!(&partial.plaintext[44..], &[1, 0, 2, 0]);

        // Moved under another key, or opened with another KEK: nothing opens.
        let moved = open_stream(&kek, "calls/c2.wav.sealed", &object).unwrap();
        assert_eq!(moved.frames, 0);
        assert!(open_stream(&Kek::from_secret(b"other"), "calls/c1.wav.sealed", &object).is_err());

        // A damaged frame stops verification there.
        let mut damaged = object.clone();
        damaged[cut_at + 10] ^= 1;
        let opened = open_stream(&kek, "calls/c1.wav.sealed", &damaged).unwrap();
        assert!(!opened.complete);
        assert_eq!(opened.frames, 2);
    }

    #[test]
    fn streamed_wav_sizes_are_fixed_on_read() {
        let mut wav = wav_streaming_header(16000);
        assert_eq!(&wav[40..44], &u32::MAX.to_le_bytes());
        wav.extend_from_slice(&[0; 10]);
        fix_wav_sizes(&mut wav);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 46);
    }
}
