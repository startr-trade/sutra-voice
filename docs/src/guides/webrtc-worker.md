# Video meetings: the WebRTC worker and meeting sessions

Sutra can sit in on a video meeting — a [LiveKit](https://livekit.io) room — record each
participant, transcribe each participant's own track, and hand the flow each participant's audio
and sampled camera frames segment by segment, as media references, for the flow to analyse
(expression, engagement, body language, emotion) with a model of its choosing. Two pieces do the
media part:

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
   call-segment (per participant, per segment: media references) ─ call-hangup (references + summary)
                                                                                      │
   the flow: policy (DMN) ─ prompt (FEEL) ─ llm channel with {ref, sha256} parts ─ fusion (FEEL)
```

1. The worker waits in the room. When the first other participant is present it opens a session:
   a `start` frame presenting the room as a call to `SUTRA_WORKER_CALLED`, which a `call-start`
   channel binds (`bind: number:…`).
2. Each participant is announced with a session **index**, its **display label** and the **role**
   it was admitted with (a room-token attribute) — never an identity, and never an inferred role.
3. Audio of each participant, and frames of each camera at the rate the channel sets, flow on the
   same socket. The flow sees none of it.
4. As each segment closes, the transport writes each segmented participant's audio and frames of
   it to the recording store (sealed) and delivers a `call-segment` message with their
   references. The flow decides whether to analyse it and calls the model.
5. When the room empties, the worker stops the session; the transport finishes (transcripts
   drained, the last segment written and delivered, recordings completed) and delivers
   `call-hangup`.

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
| `record: true` \| `after-consent` | record each participant's audio, sealed, to `SUTRA_VOIP_RECORDING_STORE`; pair it with `<q:audit capture="media">`. `after-consent`: nothing is recorded, and no segment is cut, until a `play` marked `consent: true` has played to the end (or a `record-start` says consent was given) | false |
| `video.fps` | regular frames per second per participant (fractional allowed, ≤ 30) | 1 |
| `video.extra-frames` | a frame of everyone at turn boundaries and flagged moments | true |
| `video.max-width` / `video.quality` / `video.format` | JPEG size and quality (`image/jpeg` or `image/png`) | 640 / 70 / jpeg |
| `video.enabled` | sample video at all | true |
| `silence-ms` / `dead-air-ms` / `energy-threshold` | what a turn boundary and a long silence are | 700 / 2000 / 500 |

## Channels

A **`call-participant`** channel receives `{callId, event: joined|left, participantId,
participantIndex, label, role, segmented, atMs}` (`segmented`: the participant's role is one the
`call-segment` channel cuts).

A **`call-transcript`** channel (`transcribe.channel`, as for listen-only calls) now opens one
live session **per participant** through that `llm` channel (each holds one of its `concurrency`
permits, so size it for the participants): every segment carries `speaker` = the participant's label,
`speakerSource: "track"` and `participantId`, with offsets in session time. Attribution is exact.

## Segments: media for the flow

A **`call-segment`** channel cuts the session into fixed segments and hands the flow each
participant's media of each one. The transport analyses nothing: it writes and measures.

| Key | Meaning | Default |
|---|---|---|
| `segment.seconds` | segment length (2–300) | 10 |
| `segment.max-frames` | the most sampled frames of one participant written for one segment, requested frames first (0–32; 0: audio only) | 8 |
| `segment.roles` | the roles cut into segments; anyone else's media is never written as a segment, and their frames are not even kept | every role |
| `segment.default-role` | the role of a participant admitted without one | `unknown` |

As each segment closes (a second or two after its end, so the transcript lines of it have
arrived), and once more for the last, partial one before the hang-up, the transport writes each
segmented participant's audio of the segment as a WAV file and each of its frames as an image to
`SUTRA_VOIP_RECORDING_STORE`, under
`<tenant>/<module>/<version>/<callId>/segments/p<index>/<segment>.wav` and
`…/<segment>-f<n>.jpg`. Each object is sealed (`.sealed`, the engine's single-shot sealed-object
format `SUTRA-SEALED-1`, under `SUTRA_VOIP_RECORDING_KEK`) unless the encryption is `none`. Then
one message per participant:

```json
{"callId": "standup-1", "key": "standup-1:2:3", "participantId": "PA_sam", "participantIndex": 2,
 "participant": "P2", "label": "Sam", "role": "external", "declaredRole": "external",
 "segment": 3, "startMs": 10000, "endMs": 15000, "speaking": true, "talkMs": 3120, "audioMs": 5000,
 "features": {"durationMs": 5000, "voicedRatio": 0.62, "energyDbfs": {…}, "pitchHz": {…}},
 "flaggedMoments": [{"atMs": 1200, "reason": "turn-boundary", "participant": "P1"}],
 "words": [{"speaker": "P1", "startMs": 0, "endMs": 900, "text": "…"}],
 "present": [{"participant": "P1", "role": "staff", "talkMs": 900}, …],
 "audio": {"kind": "media", "uri": "s3://…/segments/p2/3.wav.sealed", "sha256": "…",
           "participant": "PA_sam", "startMs": 10000, "endMs": 15000, "format": "audio/wav",
           "rate": 16000, "durationMs": 5000, "bytes": 160195, "sealed": true},
 "frames": [{"kind": "media", "uri": "s3://…/segments/p2/3-f1.jpg.sealed", "sha256": "…",
             "participant": "PA_sam", "startMs": 11240, "endMs": 11240, "atMs": 1240,
             "requested": true, "format": "image/jpeg", …}],
 "status": "complete"}
```

`features` are generic acoustic measurements (duration, voicing, energy and pitch contours) and
`words`, `present` and `flaggedMoments` name participants only by pseudonym. A segment whose media
could not be written says `status: "failed"` with a `detail`; with no recording store it says
`unavailable`.

**Analysing a segment is the flow's.** It decides whether to (a decision table over the declared
role, say), builds the prompt, and calls an `llm` channel with the media as references; the
channel's `media-store` names the engine's object store over the same bucket and key-encryption
key, so it reads each object, checks its SHA-256 and opens it:

```yaml
  - name: video-analysis
    direction: outbound
    transport: llm
    bind: "llm://video-analysis"
    ack-mode: on-complete
    llm:
      media-store: recordings          # sutra.object-stores.recordings: the recording bucket
      require: [video-understanding]
      candidates:
        - provider: gemini
          model: gemini-2.5-flash
          api-key: env:GEMINI_API_KEY
```

```text
messages: [{role: "user", content: "Analyse participant " + segment.participant + " …",
            audio: {ref: segment.audio.uri, sha256: segment.audio.sha256, format: segment.audio.format},
            video: {frames: for f in segment.frames return {ref: f.uri, sha256: f.sha256, format: f.format, atMs: f.atMs}}}]
```

The engine's object store is declared in the engine configuration, for example
`SUTRA_OBJECT_STORES=recordings`, `SUTRA_OBJECT_STORES_RECORDINGS_URI=s3://meeting-recordings/sessions`
(the same location as `SUTRA_VOIP_RECORDING_STORE`), `…_ENDPOINT`, `…_ACCESS_KEY`, `…_SECRET_KEY`,
and `…_KEK` (the same key as `SUTRA_VOIP_RECORDING_KEK`). The engine book's "Recordings and media
references" covers the store keys and the refusals.

`examples/video-meeting-assistant` does all of it in `segment-analysis.bpmn` with
`rules/analysis-policy.dmn`: see [Emotion analytics and compliance](emotion-analytics.md).

`call-analysis` and the `analyze.*` keys of earlier versions are refused at load, with a pointer
here.

The **`call-hangup`** of a session carries `participants` (role, `segmented`, `segments`
delivered, talk time, transcript lines, frame counts, the recording reference, the transcript
summary), `recordings` (the media references `<q:audit capture="media">` audits), `segments`
(`segmentMs`, `maxFrames`, `roles`, `delivered`, `failed`, `skipped` while waiting for consent),
`video` (`framesRetained: false`), `flaggedMoments` (a count) and `consentAnnounced`.

## Memory

What the transport holds per participant does not grow with the meeting: its audio only for a
window (the longer of 120 s and a segment plus a few seconds), its frames, transcript lines, voiced
spans and flagged moments only until the segment they fall in has been written. A 30-minute,
two-person session with 1 fps frames peaks at about 4.6 MB per participant (16 kHz), the same as
in its first five minutes. Recordings stream to the store in parts and hold at most a part.

## Compliance

- **Roles are declared, never inferred.** Whoever issues the room token sets the role attribute
  (`SUTRA_WORKER_ROLE_ATTRIBUTE`, default `role`).
- **The transport's gate:** `segment.roles` limits who is cut into segments at all; anyone else's
  media is never written as a segment, and their frames are not kept.
- **The flow's policy:** who is analysed, and why not, is a decision in the flow (the example's
  decision table: external parties; staff only with the deployment's switch AND an opt-in, since
  inferring emotions of people at work is prohibited in the EU, AI Act, Art. 5(1)(f)).
- **No identification, no sensitive attributes:** the flow's prompt names pseudonyms (`P1`, `P2`)
  and the flow keeps only the fields it asked for. See
  [Emotion analytics and compliance](emotion-analytics.md).
- **Media are referenced, sealed and erasable.** Recordings are audio only; segment audio and
  frames are sealed objects named by their SHA-256 in the flow's variables and audit events, so
  subject erasure and retention delete them.
- **Disclosure** is part of the flow: the example's bot announces recording and analysis in the
  room, and its display name says so; `record: after-consent` keeps everything off until then.

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
| `SUTRA_WORKER_MEDIA_TOKEN` | the engine's media token (`SUTRA_VOIP_MEDIA_TOKEN`) — a reference. Sent as an `Authorization: Bearer` header on the WebSocket upgrade | required |
| `SUTRA_WORKER_ENGINE_URL` | the engine's media-stream endpoint. Keep the token out of it: the engine refuses a `/voip/stream` request with `?token=` in the URL | `ws://engine:8080/voip/stream` |
| `SUTRA_WORKER_CALLED` | the number a session presents (what `call-start` binds) | required |
| `SUTRA_WORKER_IDENTITY` / `SUTRA_WORKER_NAME` | the bot's identity and visible name | `sutra-meeting-bot` / `Meeting assistant (recording)` |
| `SUTRA_WORKER_AUDIO_RATE` | audio rate to the engine | 16000 |
| `SUTRA_WORKER_ROLE_ATTRIBUTE` | the participant attribute naming the role | `role` |
| `SUTRA_WORKER_EMPTY_GRACE_MS` | how long an empty room waits before the session ends | 3000 |
| `SUTRA_WORKER_ONCE` | exit after one session | false |

The same image carries `sutra-webrtc-participant`, a scripted participant (synthetic microphone on
a talk script, synthetic camera) for tests and demos — not a client.

## Not covered yet

- Video recording (LiveKit Egress to the object store): the recordings are audio only; only the
  sampled frames of segmented participants are written, as segment media.
- Phone participants through LiveKit SIP.
- Several rooms per worker (run one worker per room).
- Live (real-provider) runs are manual: the tier-2 test uses the stub model.
