# Video meetings: the WebRTC worker and meeting sessions

Sutra can sit in on a video meeting — a [LiveKit](https://livekit.io) room — record each
participant, transcribe each participant's own track, and analyse each participant's expression,
engagement, body language and emotion per time segment, from their audio and sampled camera
frames. Two pieces do it:

- **`sutra-webrtc-worker`** — a separate binary (its own image) that joins the room as a
  **visible** participant, subscribes to everyone else's audio and video, and streams them to the
  engine. It is the only piece that links libwebrtc; the engine never does.
- **The `voip` transport's meeting sessions** — an additive extension of its generic
  media-stream protocol: one socket carries every participant, tagged.

The runnable example is `examples/video-meeting-assistant/` (compose stack, stub and live
variants, README).

## How a meeting reaches a flow

```
LiveKit room ──(WebRTC)──> sutra-webrtc-worker ──(WebSocket /voip/stream, tagged)──> engine
                                                                                      │
   call-start (session) ─ call-participant (joined/left) ─ call-transcript (per track)
   call-analysis (per participant, per segment) ─ call-hangup (references + summary)
```

1. The worker waits in the room. When the first other participant is present it opens a session:
   a `start` frame presenting the room as a call to `SUTRA_WORKER_CALLED`, which a `call-start`
   channel binds (`bind: number:…`).
2. Each participant is announced with a session **index**, its **display label** and the **role**
   it was admitted with (a room-token attribute) — never an identity, and never an inferred role.
3. Audio of each participant, and frames of each camera at the rate the channel sets, flow on the
   same socket. The flow sees none of it.
4. When the room empties, the worker stops the session; the transport finishes (transcripts
   drained, remaining segments analysed, recordings written) and delivers `call-hangup`.

## The protocol extension

A session is tagged when its `start` frame says `"streams": "tagged"`. Everything else in the
generic protocol is unchanged.

**Gateway → engine, text frames**

| Frame | Fields |
|---|---|
| `start` | as before, plus `"streams": "tagged"`, `"video": {"formats": ["image/jpeg"]}`, `headers` (`x-room`) |
| `participant-joined` | `participantId` (opaque), `index` (1–65535, unique in the session), `label`, `role` (or null), `atMs` |
| `participant-left` | `participantId`, `reason` (`left`, `disconnected`), `atMs` |
| `track` | `participantId`, `kind` (`audio`/`video`), `state` (`subscribed`/`unsubscribed`) — informational |
| `stop` | `reason` (`room-empty`, `room-closed`, `hangup`, `worker-shutdown`) |

**Gateway → engine, binary frames** — a 12-byte header, then the payload:

| Bytes | Meaning |
|---|---|
| 0 | kind: `1` audio, `2` video image |
| 1 | flags: bit 0 = this frame answers a `sample` request |
| 2–3 | participant index, u16 big-endian |
| 4–11 | capture time, ms since the session started, u64 big-endian |
| 12… | audio: PCM16 little-endian mono at the `start` format's rate; video: one JPEG |

**Engine → gateway**

| Frame | Meaning |
|---|---|
| `{"event":"policy","video":{"enabled","fps","format","maxWidth","quality"}}` | sent once the session is accepted; no video is sent before it |
| `{"event":"sample","participantId":null,"reason"}` | send a frame of every participant (or of one) now — a turn boundary, an interruption, raised energy, a long silence |
| binary | audio for the bot to speak in the room (the consent announcement) |
| `clear` / `mark` / `hangup` | as in the generic protocol; `hangup` makes the worker leave |

## Channel keys

On the **`call-start`** channel:

| Key | Meaning | Default |
|---|---|---|
| `mode: listen` | no turns are cut (a tagged session is listen-only anyway) | conversation |
| `record: true` | record each participant's audio, sealed, to `SUTRA_VOIP_RECORDING_STORE`; pair it with `<q:audit capture="media">` | false |
| `video.fps` | regular frames per second per participant (fractional allowed, ≤ 30) | 1 |
| `video.extra-frames` | a frame of everyone at turn boundaries and flagged moments | true |
| `video.max-width` / `video.quality` / `video.format` | JPEG size and quality (`image/jpeg` or `image/png`) | 640 / 70 / jpeg |
| `video.enabled` | sample video at all | true |
| `silence-ms` / `dead-air-ms` / `energy-threshold` | what a turn boundary and a long silence are | 700 / 2000 / 500 |

A **`call-participant`** channel receives `{callId, event: joined|left, participantId,
participantIndex, label, role, analysed, analysisPolicy, atMs}`.

A **`call-transcript`** channel (`transcribe.channel`, as for listen-only calls) now opens one
live session **per participant** through that `llm` channel (each holds one of its `concurrency`
permits, so size it for the participants): every segment carries `speaker` = the participant's label,
`speakerSource: "track"` and `participantId`, with offsets in session time. Attribution is exact.

A **`call-analysis`** channel turns on segment analysis:

| Key | Meaning | Default |
|---|---|---|
| `analyze.channel` | the `llm` channel of this deployment the analysis calls; it must `require: [video-understanding]` and call (not `mode: job`) — checked at load (`SUTRA.VOIP.MODEL_CHANNEL_INVALID`). Its candidates name the models (`gemini`, native API, reads frames and audio), its `fallback-on` the fallback, its `timeout-seconds` one call's deadline | required |
| `analyze.roles` | the roles analysed | `[external]` |
| `analyze.allow-staff` | allow a staff role (`staff`, `internal`, `employee`, `agent`, `host`) in `roles` | false — refused at load otherwise |
| `analyze.default-role` | the role of a participant admitted without one | `unknown` (not analysed) |
| `analyze.segment-seconds` | segment length (2–300) | 10 |
| `analyze.when` | `post-call`, or `periodic` (each segment as it closes) | post-call |
| `analyze.max-frames` | frames of one participant per segment sent to the model (requested frames first) | 8 |
| `analyze.audio` | send the participant's segment audio too, when a candidate of the channel takes audio (a candidate that cannot is then skipped for that call) | true |

The model is never configured on the voip channel: `analyze.provider`, `.model`, `.base-url`,
`.api-key` and `.timeout-ms` are refused at load. A typical channel:

```yaml
  - name: video-analysis
    direction: outbound
    transport: llm
    bind: "llm://video-analysis"
    ack-mode: on-complete            # the transport calls it inline: no reply channel
    llm:
      require: [video-understanding]
      candidates:
        - provider: gemini
          model: gemini-2.5-flash
          api-key: env:GEMINI_API_KEY
        - provider: gemini           # the fallback
          model: gemini-2.5-flash-lite
          api-key: env:GEMINI_API_KEY
```

Each answer is one message: `{callId, key: "<callId>:<index>:<segment>", participantId, role,
segment, startMs, endMs, speaking, talkMs, frames, framesRequested, audioMs, features, channel,
provider, model (the candidate that answered), status,
result: {expression, engagement, attention, bodyLanguage, emotion: {label, confidence,
modalities: {voice, face, body, words}}, disagreement: {flagged, detail}, reactions},
removedKeys}`.

The **`call-hangup`** of a session carries `participants` (role, analysed and why, talk time,
frame counts, the recording reference, the transcript summary, an analysis digest), `recordings`
(the media references `<q:audit capture="media">` audits), `analysis` (channel, the provider and
model of the latest answer, segments,
results, failures, roles), `video` (`framesRetained: false`) and `consentAnnounced`.

## Compliance, as enforced

- **Roles are declared, never inferred.** Whoever issues the room token sets the role attribute
  (`SUTRA_WORKER_ROLE_ATTRIBUTE`, default `role`).
- **External participants by default; staff only by explicit opt-in.** Inferring emotions of
  people at work is prohibited in the EU (AI Act, Art. 5(1)(f)); a staff role in `analyze.roles`
  without `analyze.allow-staff: true` fails the channel at load. Enable it only where that does not
  apply and the people concerned agreed.
- **A participant who is not analysed never reaches the model**: none of their frames or audio is
  sent; their frames are not even kept.
- **No identification, no sensitive attributes.** The model sees pseudonyms (`P1`, `P2`), is told
  not to identify anyone or infer age, gender, ethnicity, health, religion, political or sexual
  orientation, and keys carrying such inferences are stripped from its answer (`removedKeys`).
- **Frames are not retained.** They live in memory until their segment is analysed (or the
  session ends) and are never stored; recordings are audio only, sealed, referenced by hash.
- **Disclosure** is part of the flow: the example's bot announces recording and analysis in the
  room, and its display name says so.
- The analysis is **advisory** — for people to review, never an automated decision.

## Running the worker

Build and check it (the host needs only Docker — the builder installs clang 21):

```bash
make worker-image    # sutra-webrtc-worker:dev, both binaries
make worker-lint     # fmt + clippy -D warnings in Docker, cargo deny + audit
make worker-test     # unit tests in Docker
make worker-scan     # Trivy over the image
```

| Variable | Meaning | Default |
|---|---|---|
| `LIVEKIT_URL` | the LiveKit server | `ws://livekit:7880` |
| `SUTRA_WORKER_ROOM` | the room | required |
| `SUTRA_WORKER_API_KEY` / `SUTRA_WORKER_API_SECRET` | LiveKit key pair — **references** (`env:NAME`, `file:/path`); literals are refused | required |
| `SUTRA_WORKER_MEDIA_TOKEN` | the engine's media token (`SUTRA_VOIP_MEDIA_TOKEN`) — a reference | required |
| `SUTRA_WORKER_ENGINE_URL` | the engine's media-stream endpoint | `ws://engine:8080/voip/stream` |
| `SUTRA_WORKER_CALLED` | the number a session presents (what `call-start` binds) | required |
| `SUTRA_WORKER_IDENTITY` / `SUTRA_WORKER_NAME` | the bot's identity and visible name | `sutra-meeting-bot` / `Meeting assistant (recording)` |
| `SUTRA_WORKER_AUDIO_RATE` | audio rate to the engine | 16000 |
| `SUTRA_WORKER_ROLE_ATTRIBUTE` | the participant attribute naming the role | `role` |
| `SUTRA_WORKER_EMPTY_GRACE_MS` | how long an empty room waits before the session ends | 3000 |
| `SUTRA_WORKER_ONCE` | exit after one session | false |

The same image carries `sutra-webrtc-participant`, a scripted participant (synthetic microphone on
a talk script, synthetic camera) for tests and demos — not a client.

## Not covered yet

- Video recording (LiveKit Egress to the object store) — the recordings are audio only; frames are
  analysed and dropped.
- Phone participants through LiveKit SIP.
- Several rooms per worker (run one worker per room).
- Live (real-provider) runs are manual: the tier-2 test uses the stub model.
