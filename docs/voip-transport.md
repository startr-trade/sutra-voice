# Phone and VoIP calls: the `voip` transport

The `voip` transport lets a flow take part in a phone call: answer a number, hear the caller one
utterance at a time, speak back, transfer the call, hang up — and place calls of its own. It is
turn-based: the caller's audio is cut into turns and each turn is a message; the flow's answers
are played back in real time.

The transport never touches telephony codecs. A **media gateway** (a PBX such as rustpbx, or any
gateway speaking the media-stream protocol below) negotiates each call leg's codec, transcodes at
the edge, and exchanges linear PCM (or G.711) with the engine. So G.729, G.722 or Opus never
enter Sutra; it handles PCM16, G.711 µ-law / A-law and WAV, in pure Rust.

It ships in the **`sutra-voice-engine`** image (`make image` in this repository): the Sutra
engine's default bundle plus this transport. The public engine image does not include it.
Everything else a flow uses — channels, [`llm` channels](https://sutra.startr.trade/building/llm-channels.html),
wait states, capacity pools — is the engine's, documented in the [Sutra book](https://sutra.startr.trade/).

## How a call reaches a flow

```text
caller ── SIP/RTP ──▶ gateway (PBX) ── media WebSocket ──▶ engine: voip transport ──▶ channels ──▶ flow
                         ▲                                         │
                         └──────── control (rustpbx: RWI) ◀────────┘  originate · transfer · hang-up
```

- **Media** arrives on a WebSocket served over the engine's own HTTP listener (no extra port):
  `GET /voip/bridge` — the PCM bridge a PBX dials — or `GET /voip/stream` — the gateway-neutral
  media-stream protocol.
- **Turns.** An energy VAD cuts the caller's audio into utterances: speech ends a turn after
  `silence-ms` of silence, or after `max-turn-ms`. Each turn is delivered with deterministic
  acoustic features (duration, voiced ratio, pitch and energy contours, overtalk).
- **Playback** is paced in real time, 20 ms per frame, so when the caller starts speaking while
  an answer plays, the transport simply stops sending (**barge-in**) — it never pre-buffers audio
  into the gateway where it could not be taken back.
- Every message of a call carries its id in the **`x-call-id`** header, plus `x-caller`,
  `x-called` (the numbers, not SIP URIs) and `x-call-direction`. A flow correlates on
  `x-call-id`.

## Engine configuration

One gateway per engine, configured through environment variables. Secrets are references
(`env:…`, `secret:…`), never literals.

| Variable | Meaning | Default |
|---|---|---|
| `SUTRA_VOIP_GATEWAY` | `generic` (the media stream is the whole interface) or `rustpbx` | `generic` |
| `SUTRA_VOIP_RWI_URL` | rustpbx's control WebSocket, `ws://pbx:8080/rwi/v1` or `wss://…` | required for `rustpbx` |
| `SUTRA_VOIP_RWI_TOKEN` | its bearer token — a reference | required for `rustpbx` |
| `SUTRA_VOIP_RWI_CA_FILE` | extra trusted CA certificates (PEM) for a `wss://` PBX with a private CA | the platform's roots |
| `SUTRA_VOIP_PUBLIC_MEDIA_URL` | how the gateway reaches this engine, `ws://engine:8080` | `ws://localhost:8080` |
| `SUTRA_VOIP_MEDIA_TOKEN` | the token every media and webhook request must carry as `?token=` — a reference | none: every media request is refused |
| `SUTRA_VOIP_BRIDGE_RATE` | the PCM sample rate asked of the bridge (≥ 8000) | `8000` |
| `SUTRA_VOIP_TRUNK` | the gateway trunk originated calls use (a request may name another) | none |
| `SUTRA_VOIP_PROMPTS_DIR` | the directory recorded prompts are played from (`play` with `prompt`); must exist | none: a prompt play is refused |
| `SUTRA_VOIP_RECORDING_STORE` | where recorded calls go: `s3://<bucket>[/<prefix>]` (any S3-compatible store) or `memory://<bucket>` (tests) | none: a call to be recorded is refused |
| `SUTRA_VOIP_RECORDING_ENDPOINT` | the store's URL when it is not AWS (`http://store:9000`; requests are path-style) | AWS |
| `SUTRA_VOIP_RECORDING_REGION` | the store's region | `us-east-1` |
| `SUTRA_VOIP_RECORDING_ACCESS_KEY`, `SUTRA_VOIP_RECORDING_SECRET_KEY` | the store's credentials — references | required for `s3://` |
| `SUTRA_VOIP_RECORDING_CREATE_BUCKET` | create the bucket at boot when it does not exist | `false` |
| `SUTRA_VOIP_RECORDING_ENCRYPTION` | `envelope` (each recording sealed) or `none` (the store encrypts at rest) | `envelope` |
| `SUTRA_VOIP_RECORDING_KEK` | the key-encryption key recordings are sealed under — a reference | required with `envelope` |
| `SUTRA_VOIP_RECORDING_KEY_ID` | the key id a sealed recording names | `recordings` |
| `SUTRA_VOIP_RECORDING_MAX_SECONDS` | the longest recording kept (audio beyond it is not recorded) | `7200` |

The media token rides in the URL because a PBX's bridge client sends no custom headers. Use TLS
(`wss://`) between gateway and engine anywhere outside a private network.

## Channels

### Inbound

Each inbound channel declares `transport: voip` and the event it receives with `event:` (a
`call-start` channel binds numbers instead). One channel per event per package.

| Event | When | Body (`codec: json`) |
|---|---|---|
| `call-start` | a call to a bound number; starts an instance | `{callId, from, to, direction, format, gateway}` |
| `call-turn` | one utterance of the caller | `{callId, turnIndex, endReason, bargeIn, audio: {data, format: "audio/wav"}, features}` |
| `call-dtmf` | a key the caller pressed | `{callId, digit}` |
| `call-progress` | an originated call's progress | `{callId, status, …}` — `ringing`, `answered`, `connected` (media is up), `busy`, `no-answer`, `failed` |
| `call-hangup` | the call ended, for any reason | `{callId, reason, hangupBy, sipStatus, durationMs, features}`; a listen-only or recorded call adds `mode`, `transcript` and `recording` (see [Listen-only calls](#listen-only-calls)) |
| `call-transcript` | one final transcript segment of a listen-only call | `{callId, seq, key, itemId, speaker, speakerSource, startMs, endMs, text, provider, model, features}` |
| `call-keyword` | a `phrases:` entry was heard in a segment | `{callId, phrase, segmentSeq, atMs, text}` |

```yaml
channels:
  - name: call-start
    transport: voip
    bind: "number:+15551230000"   # a list (number:+1…,+1…), a prefix (number:+1555123*), or number:*
    codec: json
    silence-ms: 700               # this much silence ends a turn
    max-turn-ms: 30000            # a turn never runs longer
    dead-air-ms: 2000             # silences at least this long count as dead air

  - name: call-turn
    transport: voip
    event: call-turn
    codec: json                   # turn-body: json (default) | wav — the WAV itself, audio/wav

  - name: call-progress
    transport: voip
    event: call-progress
    codec: json
    statuses: [connected, busy, no-answer, failed]   # deliver only these (default: all)
```

**Number binding.** The most specific `bind:` wins — an exact number over the longest prefix over
the catch-all; numbers compare on their digits (`+1 (555) 123-0000` = `15551230000`). Two channels
binding the same number or prefix across the active deployments fail closed
(`SUTRA.VOIP.BIND_DUPLICATE`); a number nothing binds is hung up at once.

**Features.** A turn's `features`: `durationMs`, `voicedMs`, `voicedRatio`, `energyDbfs` and
`pitchHz` (each `{count, mean, min, max, stddev, slopePerS}`), `overtalkMs`,
`interruptedPlayback`. The hang-up's `features` cover the whole call: `observedMs`,
`callerTalkMs`, `agentTalkMs` (the flow's own audio), `callerTalkRatio`, `silenceMs`,
`deadAirMs`, `deadAirEvents`, `longestSilenceMs`, `overtalkMs`, `interruptions`, `turns`. All are
computed from the audio, with no model involved.

### Outbound: `voip://<operation>`

| Operation | Body | Answer (as a synchronous call) |
|---|---|---|
| `originate` | `{callId?, destination, callerId?, timeoutSecs?, trunk?, headers?, mode?: "listen", dtmfAfterAnswer?, dtmfDelayMs?}` | `{callId, status: "originating", mode, recorded}` |
| `play` | `{callId, audio: {data: <base64>, format}}`, or `{callId, prompt: "<name>"}` (a recorded prompt), or the audio itself as the body, with `x-call-id`; `consent: true` marks the recording/transcription announcement | `{callId, playedMs, interrupted}` once played |
| `hangup` | `{callId, reason?}` | `{callId, status: "hung-up"}` |
| `transfer` | `{callId, target}` — an extension or SIP URI on the PBX | `{callId, status: "transferred", target}` |
| `dtmf` | `{callId, digits, gapMs?}` — `0-9`, `*`, `#`, `A-D`, one every `gapMs` (250) | `{callId, sent}` — never the digits (a PIN is a secret) |

- **`play`** takes any format the audio module knows (`audio/wav`, `audio/L16;rate=<hz>`,
  `audio/PCMU`, `audio/PCMA`) and resamples it to the call's rate — so a model's spoken answer
  (`answer.audio`) plays as is. As a synchronous channel (`ack-mode: on-complete`) the task ends
  when the audio has played; header `x-barge-in: false` makes it uninterruptible.
- **Recorded prompts.** `{callId, prompt: "ivr/menu"}` plays `<SUTRA_VOIP_PROMPTS_DIR>/ivr/menu.wav`
  (`.wav` is implied when the name has no extension). A name is `/`-separated segments of
  letters, digits, `.`, `_` and `-` — never `..` — so it cannot leave the directory. The file is
  read when it is played, so a recording can be replaced without redeploying anything; a missing
  or non-WAV file fails the play (`SUTRA.VOIP.AUDIO_INVALID`).
- **`originate`**: the flow may name the call itself (`callId`), so it can correlate before the
  PBX knows the call; otherwise an id is minted from the outbox entry, so a redelivery never
  dials twice. A bare number (`+15550001111`) is dialled through the trunk.
- An operation on a call that has just ended answers `status: "ended"` rather than failing — a
  caller hanging up while the flow is mid-step is an ordinary race.

## Listen-only calls

A flow that only listens — a meeting assistant, say — originates with `mode: "listen"`: no turns
are cut and nothing barges in; the flow may still play an announcement. The media plane stays in
the transport, and two consumers share the far end's audio:

- **Recording.** A call whose originate request comes from a `<q:audit capture="media">` process
  (the engine marks the request `sutra-audit-capture: media`) is recorded: the far end's audio,
  filled with silence where the gateway sent nothing (so the recording keeps the call's clock), with
  the transport's own prompts mixed in where they played. At the hang-up it is written once, as
  `audio/wav`, to `SUTRA_VOIP_RECORDING_STORE` under `<tenant>/<module>/<version>/<callId>.wav`,
  sealed (`.sealed`: a fresh data key per recording, wrapped under `SUTRA_VOIP_RECORDING_KEK`)
  unless the encryption is `none`. The `call-hangup` carries the REFERENCE:
  `recording: {kind: "media", uri, sha256, bytes, durationMs, format, rate, channels, encryption:
  {scheme, cipher, keyId}, consentAnnounced, consentAnnouncedAtMs, truncated, callId}` — `sha256`
  is the stored object's, so its integrity can be checked without the key. The process's audit
  events carry the same reference (never the audio). A store that cannot be written answers
  `recording: {status: "failed", detail}`; a call to be recorded with no store configured is
  refused (`SUTRA.VOIP.RECORDING_UNAVAILABLE`). Recording is in memory until the hang-up (8 kHz
  mono: about 58 MB an hour), bounded by `SUTRA_VOIP_RECORDING_MAX_SECONDS`.
- **Live transcription.** A `call-transcript` channel opens a live audio session for each
  listen-only call, transcription-only (it never answers), THROUGH the `llm` channel of the same
  deployment that `transcribe.channel` names — the model, its key and any fallback are configured
  there, once, like every model call of the package (the llm channels guide, "Channels a
  transport calls"). The `llm` channel must `require: [realtime]`, call (not `mode: job`) and
  exist; otherwise the voip channel set fails at load with `SUTRA.VOIP.MODEL_CHANNEL_INVALID`, as
  does a `transcribe.provider` / `.model` / `.api-key` written on the voip channel itself. The
  audio goes from the transport to the provider; it never reaches a process. It delivers each
  final segment as one message (with `channel`, `provider`, `model`: the candidate that served); `startMs` / `endMs` are offsets into the call's audio, and
  so into its recording. `transcribe.drain-ms` (3000) is how long the session may finish after
  the hang-up; the `call-hangup` waits for it, so every segment precedes it, and reports
  `transcript: {status, segments, keywords, channel, provider, model, speakerLabels, audioMs,
  errors}`; an `llm` channel that is not live, or whose every candidate failed to open a
  session, answers `status: "unavailable"` with the reason.
  **Speakers:** OpenAI Realtime and Gemini Live label no speakers, and a conference dial-in is one
  mixed channel, so `speaker` is `"unknown"` and `speakerSource` `"none"` unless a provider labels
  one — attribute speakers afterwards (a model over the transcript) and say that it was inferred.
- **Keywords.** A `call-keyword` channel's `phrases:` (case-insensitive) raise one message each
  time a final segment contains one — sparse events for the flow, not a stream.

```yaml
  - name: call-transcript
    transport: voip
    event: call-transcript
    codec: json
    transcribe:
      channel: meeting-transcription   # an llm channel of this deployment
      language: en
  - name: meeting-transcription
    direction: outbound
    transport: llm
    bind: "llm://meeting-transcription"
    ack-mode: on-complete              # the transport calls it inline: no reply channel
    llm:
      require: [realtime]              # live audio sessions (openai *realtime*, gemini *live*)
      candidates:
        - provider: openai
          model: gpt-realtime
          transcription-model: gpt-4o-transcribe
          api-key: env:OPENAI_API_KEY  # a reference
  - name: call-keyword
    transport: voip
    event: call-keyword
    codec: json
    phrases: [action item, deadline]
```

**Joining a conference** by its SIP dial-in: originate, wait for `connected`, then send the PIN
(`voip://dtmf`, or `dtmfAfterAnswer` on the originate) — over the rustpbx bridge the digits reach
the far leg as RFC 2833 telephone events. `examples/meeting-assistant` is the whole flow.

## Meeting sessions (many participants, audio and video)

A gateway that sits in a meeting as a participant can stream every other participant on ONE
socket of the generic protocol — each participant's own audio and sampled camera frames — and the
transport records, transcribes and analyses each participant separately. The protocol extension,
the channel keys (`mode`, `record`, `video.*` on the `call-start` channel; the
`call-participant` and `call-analysis` channels) and the WebRTC worker that does this for a
LiveKit room are described in [Video meetings (WebRTC worker)](webrtc-worker.md).

## Lines: capacity pools and session-end

A trunk allows a fixed number of concurrent calls, shared by inbound and outbound. Declare it once
as an engine **capacity pool** and reference it from the channels:

```yaml
# engine configuration (or SUTRA_CAPACITY_POOLS=trunk-main, SUTRA_CAPACITY_POOLS_TRUNK_MAIN_SLOTS=30, …)
sutra:
  capacity-pools:
    trunk-main:
      slots: 30
      reserve: { inbound: 6 }      # outbound can never take the last 6
```

```yaml
  - name: call-start
    transport: voip
    bind: "number:+15551230000"
    pool: trunk-main
    pool-call-key: header.x-call-id    # one slot per call
    session-end:
      on-message: call-hangup          # the hang-up frees the line; the instance runs on

  - name: originate
    direction: outbound
    transport: voip
    bind: "voip://originate"
    pool: trunk-main
    pool-call-key: header.x-call-id
```

A call to the number takes a slot or is refused busy. An originate waits in the outbox — oldest
first, no attempt counted — until a line frees, so a batch of calls paces itself to the trunk.
The slot belongs to the call: `session-end` returns it at the hang-up even when the instance goes
on (post-call work), and End or failure return anything still held.

## Writing the flow

- **Correlate on the call id.** Give the process that starts on `call-start` a start-event alias
  over the header, and every later message of the call finds it:
  `<q:alias name="callKey" expression="getValue(header, &quot;x-call-id&quot;)" unique="true" onConflict="correlate"/>`.
  For an originated call, name the call before dialling (`callId` in the request) and alias on it.
- **A conversation is a loop.** One instance is the whole call: after each answer the flow goes
  back to the same wait — an event-based gateway on `call-turn`, `call-dtmf`, `call-hangup` and an
  idle timer — and every pass is a new wait with its own timer and correlation. Keep the
  conversation in the instance's variables. The transport delivers a call's messages strictly in
  order, one at a time, and holds the next while the instance is still busy with the last, so
  turns never overlap and a key pressed during a prompt is handled when the prompt ends. Give
  every wait of the call an arm for `call-hangup`: a message for a wait the instance is not parked
  at is refused.
- **Collect digits with a loop.** A PIN or an account number is one wait per key, looping back
  until the terminator (`#`) or a timer; a retry (invalid choice, wrong PIN) loops back to the
  prompt with a counter.
- **Wait once per attempt.** With `statuses:` on `call-progress`, an event-based gateway after an
  originate sees exactly one outcome (or its timer); a retry loops back to the dial with a
  counter, and each attempt is a new wait.

Three complete, runnable examples:

- `examples/voice-assistant/` — an inbound responder (an audio-native model, the conversation
  as one looping instance) with post-call analytics, and appointment-reminder calls with a retry
  loop.
- `examples/ivr-dtmf/` — a plain DTMF phone menu, no model: recorded prompts, a decision table
  for the menu, customer lookup and PIN entry against data stores, transfer to PBX extensions,
  and a transfer log.
- `examples/meeting-assistant/` — a listen-only bot: dials a conference, enters its PIN,
  announces recording and transcription, records (sealed, audited by reference) and transcribes
  the meeting live, and reports afterwards (minutes, decisions, action items, per-participant
  talk time and — for analysed parties only — sentiment).

## rustpbx

Checked against rustpbx 0.5.1 (`ghcr.io/restsend/rustpbx` — it publishes only a moving `latest`
tag, so pin the image by digest). Where its documentation and its source disagree, the transport
follows the source.

- **Inbound.** Route the number to rustpbx's IVR app in step mode, with the engine as its
  provider: `app = "ivr"`, `app_params = { mode = "step", url = "<engine>/voip/rustpbx/ivr-step?token=<media token>" }`.
  The engine answers every step with a media bridge to itself. Add the webhook tap so the engine
  learns of the call and takes control of it over RWI:
  `[rwi_webhook] url = "<engine>/voip/rustpbx/webhook?token=<media token>"`, events
  `call_created`, `call_hangup`. (rustpbx announces no inbound call on the RWI socket itself.)
- **Outbound.** `call.originate` through a trunk (`SUTRA_VOIP_TRUNK`), then the call is
  transferred to the engine's media bridge once answered. The trunk must list its codecs
  (`codec = ["pcmu", "pcma"]`): rustpbx builds an originated call's offer from them and refuses the
  call without.
- **Hang-up** always goes over RWI: rustpbx drops the media socket without a close frame when a
  call ends, and closing the engine's side alone would leave a silent call up.
- A trunk is recognised by the SOURCE address of the carrier's signalling (`inbound_hosts`).
- **RTP** uses rustpbx's `rtp_start_port`–`rtp_end_port`; keep the range small and publish exactly
  it (UDP) when a carrier reaches the PBX from outside its network.
- **Video** is not available through rustpbx: its media bridge is audio only.

## Limits

- Turn detection needs a continuous media stream: a gateway that sends nothing during silence
  (no RTP during silence suppression) ends a turn only when audio resumes or at `max-turn-ms`.
- One gateway per engine process.
- Realtime speech-to-speech (media straight between the call and a model's live API, the flow as
  the control plane) is not part of this transport.

## Diagnostics

`SUTRA.VOIP.BIND_INVALID`, `SUTRA.VOIP.BIND_DUPLICATE` (number binding), `SUTRA.VOIP.CHANNEL_INVALID`
(a channel's settings), `SUTRA.VOIP.CONFIG_INVALID` (the environment), `SUTRA.VOIP.OUTBOUND_INVALID`
(a request), `SUTRA.VOIP.CALL_UNKNOWN`, `SUTRA.VOIP.CALL_NOT_CONNECTED` (no media yet — retried),
`SUTRA.VOIP.AUDIO_INVALID`, `SUTRA.VOIP.GATEWAY_UNAVAILABLE` (retried), `SUTRA.VOIP.GATEWAY_REJECTED`,
`SUTRA.VOIP.NOT_RUNNING`, `SUTRA.VOIP.RECORDING_UNAVAILABLE` (a call to be recorded, no store).
