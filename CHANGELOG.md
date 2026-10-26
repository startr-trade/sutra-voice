# Changelog

All notable changes to **sutra-voice** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

sutra-voice is released on its own train, each release pinned to an exact
[Sutra engine](https://github.com/startr-trade/sutra) commit (the `engine/` submodule). Engine
changes — `llm` channels, audio in the provider SPI, capacity pools, wait states, the object
store, `q:audit capture="media"` — are in the
[engine changelog](https://github.com/startr-trade/sutra/blob/main/rust/CHANGELOG.md).

## [0.6.0] — Unreleased

The first release. The code below was built inside the engine repository during the 0.6.0 cycle
and moved here, before that release, as a separate repository: the engine stays free of
telephony and video, and this repository adds them through the engine's transport SPI.

Voice, meeting and video support is a **preview** in this release.

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

### Added — video meetings: a WebRTC media worker, per-participant sessions, segment analysis

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
- **Segment analysis** (`call-analysis` channel, `analyze.*`): fixed time segments; for each
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

### Added — the meeting assistant: listen-only calls, recordings, live transcription

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

### Added — voice calls: the `voip` transport and the voice-assistant example

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
