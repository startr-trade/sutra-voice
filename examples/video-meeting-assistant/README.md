# Video meeting assistant

A bot sits in a video meeting (a [LiveKit](https://livekit.io) room). It records each participant,
transcribes each participant's own track, and analyses how each participant appears and sounds,
segment by segment. That covers expression, engagement and attention, body language, and a
combined emotion estimate that says which signals support it and flags when they disagree. After
the meeting the flow stores a session record and a rendered report.

The engine never touches WebRTC. `sutra-webrtc-worker` (in `tools/`) joins the room as a
**visible** participant and streams every other participant to the engine, using the `voip`
transport's media-stream protocol. Protocol and channel keys: the guide page "Video meetings
(WebRTC worker)".

## What runs

| Service | Role |
|---|---|
| `livekit` | LiveKit server in dev mode (key `devkey` / secret `secret`, single node, no Redis) |
| `worker` | the bot, named "Meeting assistant (recording)" and visible to everyone in the room |
| `engine` | Sutra with this package; the `voip` transport serves `/voip/stream` |
| `recordings` | MinIO, the S3-compatible bucket for the sealed per-participant recordings |
| `model-stub` | stands in for the live transcription sessions and the Gemini video model |
| `engine-db` | PostgreSQL |
| `alex`, `sam` (`--profile demo`) | two scripted participants (synthetic camera and microphone): a test and demo tool |

## Run it (stub, no keys)

```bash
make worker-image
sutra package examples/video-meeting-assistant/variants/video-stub \
    --overlay examples/video-meeting-assistant/shared \
    --out examples/video-meeting-assistant/deploy/deployments
docker compose -f examples/video-meeting-assistant/deploy/compose.yaml up -d
docker compose -f examples/video-meeting-assistant/deploy/compose.yaml --profile demo up -d alex sam
```

About 20 seconds later the session is over. Read it back:

```bash
docker compose -f examples/video-meeting-assistant/deploy/compose.yaml exec engine-db \
  psql -U postgres -d sutra -c "SELECT store_name, store_key FROM data_store ORDER BY 1, 2"
```

The output has these stores:

- `meetings` / `session:<callId>`: the record (participants, roles, who was analysed and why, talk time, each recording's URI and SHA-256, the analysis digest, the rendered report).
- `transcripts` / `<callId>:<seq>`: one segment each, `speaker` = the participant's label, `speakerSource: "track"`.
- `analysis` / `<callId>:<index>:<segment>`: one per segment of each participant cut into segments: the fused analysis, or `not-analysed` and why.

To join yourself, use any LiveKit client against `ws://127.0.0.1:7880` with a token for room
`standup`. Mint it with the dev key, and give it a `role` attribute (`external` or `staff`).
LiveKit's `lk token create` command or any server ## Where the analysis lives

The transport handles media only. As each 5 s segment closes it writes the segment's audio (a WAV
file) and its sampled frames to the recording bucket, each one sealed, and delivers a
`call-segment` message with their **references** and the segment's generic measurements (talk
time, acoustic features, flagged moments, the words spoken, who was present). Everything that
decides or interprets is in the flows:

| File | What it does |
|---|---|
| `shared/rules/analysis-policy.dmn` | who is analysed: external parties; staff only with the deployment's switch (`allowStaff`, false as shipped) and an opt-in; anyone else not |
| `shared/bpmn/segment-analysis.bpmn` | per segment: the policy, the prompt (FEEL), the model call through `video-analysis` with `{ref, sha256}` parts, the fusion (only the fields asked for, only the modalities sent, disagreement kept), the row |
| `shared/bpmn/video-meeting.bpmn` | the session: the roster with each participant's policy, then the rows read back and the digest (dominant emotion and engagement, disagreements) for the report |

The `video-analysis` channel names the engine's object store `recordings` as its `media-store`;
the compose file declares it over the same bucket with the same key-encryption key, so the channel
reads each referenced object, checks its SHA-256 and opens it before the call.

## Live models

Use `variants/video-live`. The models are two `llm` channels: `meeting-transcription` (OpenAI
Realtime, falling back to Gemini Live; `require: [realtime]`), which the transport opens a live
session on for each participant's track, and `video-analysis` (the native Gemini API, which reads
image frames and audio, falling back to a lighter Gemini model; `require: [video-understanding]`),
which `segment-analysis.bpmn` calls. Export `OPENAI_API_KEY` and `GEMINI_API_KEY` before `up`.
Secrets are references (`env:…`), never literals, in the engine and in the worker.

For a real LiveKit deployment, replace the dev key pair:

- On the server, set your own keys.
- On the worker, set `SUTRA_WORKER_API_KEY` / `SUTRA_WORKER_API_SECRET` as `env:` or `file:` references.

The worker also needs the engine's media token (`SUTRA_WORKER_MEDIA_TOKEN`), which it sends as an
`Authorization: Bearer` header. Keep it out of `SUTRA_WORKER_ENGINE_URL`.

## Consent and compliance

- **Disclosure.** The bot's name says it records. At the start of the session the flow plays
  the announcement into the room (`prompts/consent.wav`). Replace it with a real recording that
  says the meeting is recorded, transcribed and analysed. The session is `record: after-consent`:
  nothing is recorded, and no segment is cut, until the announcement has played to the end. Each
  recording reference reports it (`consentAnnounced`). Check your jurisdiction's consent rules
  (one-party or all-party).
- **Who is analysed.** Two gates, both declared, neither inferred:
  - the `call-segment` channel's `roles: [external]`: only these roles are cut into segments at
    all, so a staff participant's media is never written as a segment;
  - the flow's decision table, applied to every segment and to the roster: external yes, staff
    only with the switch and an opt-in, anyone else (`unknown`, no declared role) no. A participant
    who is not analysed never has media sent to a model; the row says why.
- **Staff.** Inferring the emotions of people **at work** is prohibited in the EU (AI Act,
  Art. 5(1)(f)). Customers and other external parties may be analysed, with transparency. Turn
  `allowStaff` on (in both BPMN files) and add the staff role to `segment.roles` only where the
  prohibition does not apply and the people concerned opted in.
- **What is inferred:**
  - per analysed participant and segment: expression, engagement, attention, body-language cues
    (posture, gestures, nods), and their reaction to what others said, also while listening;
  - an emotion label with a confidence, naming the signals (voice, face, body, words) that
    support it: a signal that was not sent cannot support it (`unsupportedClaims`), and an emotion
    no sent signal supports is withheld;
  - whether those signals disagree (reported, never averaged).
- **What is never inferred or kept:**
  - identity: the model sees only pseudonyms (`P1`, `P2`);
  - age, gender, ethnicity, health, religion, political or sexual orientation: the flow keeps only
    the fields it asked for, and `removedKeys` lists anything else the model returned;
  - video recordings: the recordings are audio only, one per participant. Segment frames are
    written only for segmented participants, sealed, and referenced by hash like the audio.
- **Advisory only.** Analysis is for people to review, never an automated decision about anyone.
  The stores carry the `personal` data class, and `segment-analysis.bpmn` audits at
  `capture="media"`, so erasure and retention reach the rows and the segment objects they
  reference.

## Cost

Video sampling is the cost control, set on the `session-start` channel:

- `video.fps` (1 by default);
- `video.extra-frames`: one frame of everyone at turn boundaries and flagged moments (an
  interruption, raised energy, a long silence).

On `call-segment`, `seconds` and `max-frames` bound what each model call receives; the flow can
skip segments (a silent participant, say) before it calls the model.

s each segment as it closes rather than after the meeting.

## Not in this example

- **Video recording.** LiveKit Egress to the same bucket needs Redis and an Egress container;
  it is a follow-up.
- **Phone participants** via LiveKit SIP.
- **A minutes summary.** `examples/meeting-assistant` shows the transcript-to-minutes report job.
