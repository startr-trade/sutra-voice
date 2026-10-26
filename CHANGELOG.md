# Changelog

All notable changes to **sutra-voice** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

sutra-voice is released on its own train, each release pinned to an exact
[Sutra engine](https://github.com/startr-trade/sutra) commit (the `engine/` submodule). Engine
changes — `llm` channels, audio in the provider SPI, capacity pools, wait states, the object
store, `q:audit capture="media"` — are in the
[engine changelog](https://github.com/startr-trade/sutra/blob/main/rust/CHANGELOG.md).

## [Unreleased]

Nothing yet.

## [0.6.0] — 2026-10-26

The first release, pinned to engine `v0.6.0` (`f38abcb`). The code below was built inside the
engine repository during the 0.6.0 cycle and moved here, before that release, as a separate
repository: the engine stays free of telephony and video, and this repository adds them
through the engine's transport SPI.

Voice, meeting and video support is a **preview** in this release.

> **PREVIEW.** Everything in this release — voice calls, IVR, meetings and video — is a
> preview: the channel keys, the media protocol, the `voip://` operations and the recording
> format may still change in a minor release.

### Added — the release mechanism

- **`release.yml`** on a `v*` tag publishes `ghcr.io/startr-trade/sutra-voice` and
  `ghcr.io/startr-trade/sutra-webrtc-worker` (linux/amd64 + linux/arm64, built natively,
  cosign-signed, provenance, SPDX SBOM) and a GitHub Release from `.github/releases/v<version>.md`,
  behind four gates: CI passed on the commit, both workspace versions match the tag, `engine/` pins
  exactly a released engine tag of the same minor, and Trivy is clean on each image before it is
  tagged. Plus `trivy.yml`, `codeql.yml`, `nightly.yml` (tier 2 and the worker) and `docs.yml`
  (the book to voice.sutra.startr.trade); CI also checks the book and the worker's lockfile. The
  worker image builds for arm64 as well as amd64.

### Changed — meeting analysis moves into the flows; the engine's object store and audit journal (PREVIEW)

- **The transport no longer analyses meetings.** The `call-analysis` channel and its `analyze.*`
  keys are removed (refused at load with a pointer to the replacement). A **`call-segment`**
  channel (`segment.seconds` 2–300, default 10; `segment.max-frames` 0–32, default 8;
  `segment.roles`, default every role; `segment.default-role`) cuts a meeting session into fixed
  segments; as each closes, each segmented participant's audio of it (a WAV file) and its sampled
  frames are written to the recording store as sealed objects (the engine's single-shot
  `SUTRA-SEALED-1`, which an `llm` channel's `media-store` opens with the same key-encryption
  key) and one `call-segment` message delivers their media references (`{kind: "media", uri,
  sha256, participant, startMs, endMs, …}`) with generic measurements: talk time, acoustic
  features, flagged moments, the words spoken (pseudonymised speakers), who was present. The
  prompt, the policy (who is analysed), the model call and the fusion of the signals are the
  flow's. `call-participant` says `segmented` instead of `analysed` / `analysisPolicy`; the
  meeting `call-hangup` carries `segments` (delivered, failed, skipped) instead of `analysis`.
- **Bounded memory per meeting participant**: its audio is kept only for a window (the longer of
  120 s and a segment plus its lag), and frames, transcript lines, voiced spans and flagged
  moments are let go behind each written segment, so a participant's footprint does not grow with
  the meeting (a 30-minute, two-person session peaks at about 4.6 MB per participant, as in its
  first five minutes). A segment's acoustic features are computed off the async workers.
- **`examples/video-meeting-assistant`**: `segment-analysis.bpmn` decides with
  `rules/analysis-policy.dmn` (external parties; staff only with the deployment's `allowStaff`
  switch, false as shipped, and an opt-in), builds the prompt in FEEL, calls `video-analysis`
  with `{ref, sha256}` parts (`media-store: recordings`, an engine object store over the
  recording bucket), keeps only the fields it asked for (`removedKeys`) and the modalities that
  were sent (`unsupportedClaims`), keeps disagreement as reported, and stores the row;
  `video-meeting.bpmn` applies the same policy to the roster, reads the rows back and computes
  each participant's digest. The session is `record: after-consent`; `segment.roles: [external]`
  keeps a staff participant's media from ever being written as a segment.
  **`examples/meeting-assistant`** uses the same decision table for its internal-staff policy.
- **`record: true | "after-consent"` on `voip://originate`**: a `capture=media` leg (or one
  asking `record: true`) can wait for consent like a `record: after-consent` call — the
  recording starts only once a `play` marked `consent: true` has played to the end. The answer
  says `record`. The meeting-assistant example dials `record: "after-consent"`.
- **The engine's object store** replaces the transport's own S3 client (`store.rs`,
  `RecordingStore`, `S3RecordingStore`, `MemoryRecordingStore` are removed): recordings use
  `sutra_objectstore::{ObjectStore, S3ObjectStore, MemoryObjectStore}` — multipart uploads,
  ranged reads, listings — with the same 5 MiB parts, sealed one-second chunks, spool, sweep,
  manifests and upload markers. The transport no longer depends on `reqwest`.
- **Audit events through the engine's transport-audit journal**: the transport answers
  `writes_audit_events() = true`, and writes `voip.ORIGINATE_REFUSED`,
  `voip.ORIGINATE_RATE_LIMITED`, `voip.CONSENT_REQUIRED`, `voip.RECORDING_RECOVERED` and
  `voip.RECORDING_RECOVERED_DELIVERED` events to the journal of the instance that owns the call
  (the routing is kept with the call and in each recording's manifest, so a recovery after a
  restart still finds it), or to the deployment's. They replace the `sutra.audit` log lines.
  Fields are written at `payload` level only; a refused destination is masked and sensitive.
- **Engine pin**: `f38abcb`, the engine's `v0.6.0` release (multipart / ranged / listing object
  store, transport audit events).
- **Tests**: tier 1 — `audit_events` (refusals, consent and recoveries in the owning instance's
  journal), `a_long_meeting_holds_bounded_memory_per_participant`,
  `an_after_consent_session_cuts_no_segment_before_consent`,
  `an_originated_after_consent_call_records_only_once_the_announcement_has_played`, the segment
  settings and refusals, and `analysis_flows` (the decision table and the flows' FEEL evaluated
  with the engine's own evaluators); tier 2 — the MinIO recording suite on the engine's
  `MinioContainer`, the video meeting with flow-side analysis by reference, and
  `voip.ORIGINATE_REFUSED` in the instance's `audit_event` rows.

### Added — durable recordings, consent, credentials out of URLs, toll-fraud controls (PREVIEW)

- **Recordings stream to the store while the call is up** (no longer held in memory until the
  hang-up): an S3 multipart upload in 5 MiB parts, each one-second chunk sealed on its own
  (`sutra-sealed-stream-1`: a fresh data key per recording, wrapped under the engine's
  envelope-encryption KEK; each frame authenticated with its index, the object key and whether it
  is the last), so a truncated object verifies up to its last good chunk and reads as incomplete.
  Memory per recording is bounded by one part. Parts in progress are spooled to disk, sealed
  (`SUTRA_VOIP_RECORDING_SPOOL_DIR`). The media reference gains `complete`, `startMs` and
  `encryption.scheme: "sutra-sealed-stream-1"`. `sutra_transport_voip::recording::open_stream`
  reads a recording back.
- **Crash recovery**: a sweep at start-up and every `SUTRA_VOIP_RECORDING_SWEEP_SECONDS` (600)
  completes the uploads an earlier run left (uploading what the spool still held), verifies them
  frame by frame, amends each recording's manifest (`<key>.json`: `complete: false`, the captured
  duration, the stored object's SHA-256) and delivers the reference on the call's `call-hangup`
  channel (`reason: "recording-recovered"`, `recovered: true`) so the process's audit trail
  records it. `SUTRA_VOIP_INSTANCE_ID` names the writer (default `HOSTNAME`).
- **Recording inbound calls, behind consent**: `record: after-consent` on a `call-start` channel
  starts the recording only once an announcement marked `consent: true` has played to the end;
  `record: true` records from the moment the media connects and leaves consent to the deployment
  (an announcement the PBX plays first). New operations **`voip://record-start`** — refused with
  `SUTRA.VOIP.CONSENT_REQUIRED` until consent is given (that announcement, or the request saying
  `consent: true`) — and **`voip://record-stop`** (answers the reference). The hang-up carries
  every recording of the call in `recordings`. The voice-assistant example records its inbound
  calls after the greeting.
- **No credential in a media URL**: the PCM bridge URL a PBX is handed carries a token minted for
  that one call (HMAC-SHA256 over the call id, expiry and a nonce; valid
  `SUTRA_VOIP_BRIDGE_TOKEN_TTL_SECONDS`, 60; good for one socket) instead of the media token,
  which a bridge socket no longer accepts in its URL. The media stream (`/voip/stream`) takes the
  media token only as `Authorization: Bearer` — a `?token=` is refused — and the WebRTC worker
  sends it so. Logged URLs and gateway errors have their tokens redacted.
- **Toll-fraud controls on `voip://originate`**: nothing is dialled until
  `SUTRA_VOIP_ORIGINATE_ALLOW` lists what may be (glob patterns over the number and the SIP URI;
  **deny-all by default**); premium-rate prefixes (`SUTRA_VOIP_ORIGINATE_BLOCK_PREFIXES`, a
  built-in list) and international numbers (outside `SUTRA_VOIP_ORIGINATE_HOME_PREFIX`) are
  refused unless named specifically or switched on (`_ALLOW_PREMIUM`, `_ALLOW_INTERNATIONAL`);
  rate limits per destination (`_RATE_PER_DESTINATION`, 10 an hour) and overall
  (`_RATE_GLOBAL`, 60 a minute). Every refusal is a diagnostic (`SUTRA.VOIP.ORIGINATE_REFUSED`,
  or the retryable `SUTRA.VOIP.ORIGINATE_RATE_LIMITED`) and an audit event (see above).

### Added — the repository

- **`sutra-voice-dist`** → the `sutra-voice-engine` binary and image: the engine's default
  bundle (formats, envref resolvers, the transports and model providers, behind the same cargo
  features as the engine's `sutra-dist`) plus the `voip` transport. `make image`,
  `make scan-image`.
- **`sutra-voice-conformance`**: the four tier-2 suites below, driven through the engine's
  `sutra_testkit::conformance` harness, and a tier-1 lint of every example variant.
- The engine as a git submodule at `engine/`, consumed as path dependencies.

### Changed — the voice transport's model calls go through `llm` channels

- **`voip` transcription and segment analysis call a named `llm` channel**, never a provider
  directly: `transcribe.channel` and `analyze.channel` name an outbound `llm` channel of the same
  deployment, so the channel's ordered candidates and fallback, `require:` filtering, key
  references, `concurrency` limit and `sutra.llm.call` span apply, and models are configured in one
  place. The media still goes from the transport to the provider and never through a process.
  Checked at load (`SUTRA.VOIP.MODEL_CHANNEL_INVALID`): the channel exists in the deployment, is
  an outbound `transport: llm` channel that calls (not `mode: job`), and declares
  `require: [realtime]` (transcription) or `require: [video-understanding]` (analysis). The direct
  settings — `transcribe.{provider, model, transcription-model, base-url, api-key}` and
  `analyze.{provider, model, base-url, api-key, timeout-ms}` — are removed and refused. Transcript
  segments, analysis messages and the hang-up summaries carry `channel` plus the `provider` /
  `model` of the candidate that served.
- **Examples**: `meeting-assistant` declares `meeting-transcription`; `video-meeting-assistant`
  declares `meeting-transcription` and `video-analysis` (live: OpenAI Realtime then Gemini Live;
  `gemini-2.5-flash` then `gemini-2.5-flash-lite`).

### Added — video meetings: a WebRTC media worker, per-participant sessions, segment analysis (PREVIEW)

- **`sutra-webrtc-worker`** (`tools/sutra-webrtc-worker`, its own Cargo workspace and image —
  the engine never links libwebrtc): joins a LiveKit room as a visible participant, subscribes to
  every other participant's audio and video, and streams them to the engine on one tagged
  media-stream socket; announces each participant with a session index, its display label and its
  declared role (a token attribute) — never an identity; samples video at the rate the engine
  sets (JPEG); speaks the engine's audio into the room (the consent announcement). LiveKit keys and
  the media token are `env:` / `file:` references; literals are refused. The same image carries
  `sutra-webrtc-participant`, a scripted test/demo participant. `make worker-image`,
  `worker-lint`, `worker-test`, `worker-scan` (all in Docker; the host needs no clang).
- **`voip` meeting sessions** (additive protocol extension): a `start` with `"streams":"tagged"`
  carries every participant on one socket — `participant-joined` / `participant-left` / `track`
  frames, and binary frames with a 12-byte header (kind, flags, participant index, capture time)
  for each participant's audio and sampled frames; the engine sends `policy` (the channel's video
  sampling) and `sample` (a frame of everyone now, at turn boundaries, interruptions, raised
  energy, long silences). Per participant: its own audio tap, recorded at the end (sealed, one
  object per participant, referenced by hash in `call-hangup.recordings`), its own live
  transcription session (`speakerSource: "track"` — exact attribution), speech tracking.
  `call-start` keys `mode: listen`, `record: true`, `video.{fps, enabled, format, max-width,
  quality, extra-frames}`; new inbound kinds `call-participant` and `call-analysis`.
- **Segment analysis** (superseded before release by `call-segment` and flow-side analysis, see
  above) (`call-analysis` channel, `analyze.*`): fixed time segments; for each
  segment and each participant whose role is analysed, its segment audio, its frames (requested
  ones first) and the words spoken go to a model with video understanding (through the `llm`
  channel `analyze.channel` names); the answer — expression, engagement, attention, body-language
  cues, a fused emotion naming its supporting modalities and confidence, modality disagreement —
  is one `call-analysis` message. Post-call or periodic. Policy enforced in the transport: `roles`
  default `[external]`; a staff role is refused at load without `allow-staff: true` (the EU
  workplace prohibition); a non-analysed participant's frames are never kept or sent; the model
  sees pseudonyms; sensitive-attribute keys are stripped from answers (`removedKeys`); frames are
  never stored.
- **`examples/video-meeting-assistant`**: LiveKit (dev mode) + the worker + engine + MinIO + stub
  model compose stack, stub and live (OpenAI Realtime + native Gemini) variants, a session flow
  (consent announcement, roster loop, report), per-segment transcript and analysis stores. The
  stub model (`examples/model-stub`) answers Gemini's native `generateContent` (a segment
  analysis echoing the frames and audio it received).
- **Tests**: tier 2 — `tc_video_meeting_assistant_`.

### Added — the meeting assistant: listen-only calls, recordings, live transcription (PREVIEW)

- **`voip` listen-only calls.** `voip://originate` takes `mode: "listen"` (no turns, no barge-in)
  and `dtmfAfterAnswer`; `voip://dtmf` paces its digits (`gapMs`, default 250) and no longer
  echoes them; `voip://play` takes `consent: true`. A call whose originate carries the
  `capture=media` mark (the engine's `q:audit capture="media"`) is recorded: the far end's audio
  (silence-filled to the call's clock, the bot's own prompts mixed in) is written at the hang-up
  to `SUTRA_VOIP_RECORDING_STORE` (`s3://…` or `memory://…`), sealed under
  `SUTRA_VOIP_RECORDING_KEK` unless `SUTRA_VOIP_RECORDING_ENCRYPTION=none`, and the `call-hangup`
  carries the reference (uri, SHA-256 of the stored object, duration, format, encryption key id,
  `consentAnnounced` and where). A recorded call with no store configured is refused
  (`SUTRA.VOIP.RECORDING_UNAVAILABLE`). New inbound channels: `call-transcript` (its
  `transcribe.*` keys open a transcription-only live session through the `llm` channel
  `transcribe.channel` names; one message per final segment: seq, speaker, start/end, text,
  per-segment acoustic features) and `call-keyword` (`phrases:`). Speakers are reported as
  `"unknown"` — the live sessions label none.
- **`examples/meeting-assistant`**: join a conference by its SIP dial-in (HTTP request, or a
  timer-started standing meeting), enter the PIN as DTMF, announce recording and transcription,
  listen; store each transcript segment in a projected (XSD-structured) store; after the call,
  read the transcript back, ask for the minutes as a JSON background job, apply the analysis
  policy (external participants analysed; internal staff only when the deployment allows it and
  the request opts in — the EU workplace note in the README), render the report and store and
  send it with the recording's reference. Stub and live variants, compose stack with rustpbx,
  SIPp as the conference, MinIO, the stub model (a Realtime transcription WebSocket and a
  meeting-report job).
- **Tests**: tier 1 — the listen-only hub end to end with a scripted gateway, session and
  in-memory store, recording settings, both example variants linted; tier 2 —
  `tc_meeting_assistant_joins_records_transcribes_and_reports`.

### Added — voice calls and IVR: the `voip` transport, the voice-assistant and ivr-dtmf examples (PREVIEW)

- **`voip` transport** (`sutra-transport-voip`, linked into `sutra-voice-engine`). Turn-based
  phone and VoIP calls driven by a flow, through a media gateway that transcodes at the edge —
  Sutra only ever handles PCM16, G.711 and WAV, in pure Rust. Media arrives on a WebSocket over
  the engine's shared listener (a gateway-neutral media-stream protocol, or the PCM bridge a PBX
  dials); an energy VAD cuts the caller's speech into turns, each with deterministic acoustic
  features; playback is paced in real time, so speech during playback interrupts it (barge-in).
  Inbound channels `call-start` (bound to numbers: `number:+1555…`, lists, prefixes, `number:*`;
  most specific wins, duplicates fail closed), `call-turn`, `call-dtmf`, `call-progress`,
  `call-hangup`; outbound `voip://originate`, `play`, `hangup`, `transfer`, `dtmf`, as outbox
  sends or synchronous calls. Gateways: `generic` (the media stream only) and `rustpbx` (RWI call
  control over `ws://` or `wss://` — TLS through rustls with the platform roots plus
  `SUTRA_VOIP_RWI_CA_FILE` — its webhook tap and IVR step provider for inbound calls,
  `call.originate` with a trunk for outbound). Engine-wide settings are `SUTRA_VOIP_*`
  environment variables; diagnostic codes `SUTRA.VOIP.*`.
  - A `call-turn` is JSON by default (`turn-body: json`): the utterance as base64 WAV in the shape
    a model channel takes (`audio: {data, format}`), with its features; `turn-body: wav` delivers
    the WAV itself.
  - `statuses:` on a `call-progress` channel delivers only the named outcomes (e.g. `connected,
    busy, no-answer, failed`), so a flow waits once per call attempt.
  - A bare number is dialled through the trunk (`sip:<number>@<trunk>`); `x-caller` /
    `x-called` carry numbers, not SIP URIs.
  - An operation on a call that has just ended answers `status: "ended"` instead of failing the
    flow that raced the hang-up; a delivery the engine asks to have redelivered (the instance is
    mid-step) is retried in order.
- **Recorded prompts**: a `voip://play` request may name a prompt instead of carrying audio —
  `{callId, prompt: "<name>"}` plays `<SUTRA_VOIP_PROMPTS_DIR>/<name>.wav` (any WAV the audio
  module reads, resampled to the call's rate). A name is relative path segments of letters,
  digits, `.`, `_` and `-`, so it cannot leave the directory; a missing or non-WAV file fails the
  play permanently (`SUTRA.VOIP.AUDIO_INVALID`), and a prompt play with no directory configured
  is refused (`SUTRA.VOIP.OUTBOUND_INVALID`). The directory must exist when set
  (`SUTRA.VOIP.CONFIG_INVALID` at boot).
- **Example `examples/voice-assistant/`**: an agentic responder on a number (greeting, then the
  conversation as ONE instance looping back to its in-call wait — each utterance to an
  audio-native model, barge-in, DTMF fallback, hand-over to an agent, hang-up freeing the trunk
  line), post-call analytics (acoustic metrics plus the customer's emotion and sentiment from an
  audio-understanding model, stored per interaction; first-contact resolution and repeat contact
  decided by a DMN table over a data-store lookup by caller number), and appointment-reminder
  calls originated on the shared `trunk-main` capacity pool with a retry loop (a counter, two
  attempts) when the line is busy. Runs locally with rustpbx and SIPp (`deploy/compose.yaml`).
  Emotion analysis covers the customer only; agent-side analysis is opt-in (the README covers the
  EU rule on inferring emotions at work).
- **Example `examples/ivr-dtmf/`**: a plain DTMF phone menu with no model — recorded prompts
  (synthesised stand-in tones, with a script and instructions to swap in real recordings), a DMN
  menu table giving each key an extension and an access rule (`open` / `customers-only`), the
  menu replayed on an invalid or missing choice (a loop back to the same wait, 3 tries), a
  customer lookup by caller number, PIN entry key by key until `#` (a loop per key and per
  attempt) checked against active and expiry, transfer to rustpbx extensions that register with
  SIP digest, and a `transfer_log` of every transfer, refused PIN and abandoned call — never the
  digits. Its three stores are XSD-structured tables whose DDL and example rows ship as
  idempotent migrations. The README lists the seeded data path by path, and states the clear-text
  PIN risk with the production alternative (a synchronous call to a verification service that
  keeps salted hashes). Runs locally with rustpbx and SIPp (`deploy/compose.yaml`).
- **Tests**: tier 2 — `tc_voice_assistant_`, `tc_ivr_dtmf_`.
