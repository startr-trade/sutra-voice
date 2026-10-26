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
- `analysis` / `<callId>:<index>:<segment>`: one per analysed participant and segment.

To join yourself, use any LiveKit client against `ws://127.0.0.1:7880` with a token for room
`standup`. Mint it with the dev key, and give it a `role` attribute (`external` or `staff`).
LiveKit's `lk token create` command or any server SDK does this. A participant with no role
is not analysed.

## Live models

Use `variants/video-live`. The models are two `llm` channels the transport calls (the media goes
from the transport to the provider, never through a process): `meeting-transcription` (OpenAI
Realtime, falling back to Gemini Live; `require: [realtime]`) and `video-analysis` (the native
Gemini API, which reads image frames and audio, falling back to a lighter Gemini model;
`require: [video-understanding]`). The voip channels only name them (`transcribe.channel`,
`analyze.channel`). Export `OPENAI_API_KEY` and `GEMINI_API_KEY` before `up`. Secrets are references (`env:…`), never literals, in the engine and in the worker.

For a real LiveKit deployment, replace the dev key pair:

- On the server, set your own keys.
- On the worker, set `SUTRA_WORKER_API_KEY` / `SUTRA_WORKER_API_SECRET` as `env:` or `file:` references.

The worker also needs the engine's media token (`SUTRA_WORKER_MEDIA_TOKEN`).

## Consent and compliance

- **Disclosure.** The bot's name says it records. At the start of the session the flow plays
  the announcement into the room (`prompts/consent.wav`). Replace it with a real recording that
  says the meeting is recorded, transcribed and analysed. Each recording reference reports
  whether the announcement played to the end (`consentAnnounced`). Check your jurisdiction's
  consent rules (one-party or all-party).
- **Who is analysed.** The `call-analysis` channel decides, and the transport enforces it before
  any frame reaches a model:
  - `roles: [external]` by default.
  - A participant's role is what their room token declares, never inferred.
  - A participant with no role counts as `unknown` and is not analysed.
- **Staff.** Inferring the emotions of people **at work** is prohibited in the EU (AI Act,
  Art. 5(1)(f)). Customers and other external parties may be analysed, with transparency. A staff
  role (`staff`, `internal`, `employee`, `agent`, `host`) in `roles` is refused at load unless
  `allow-staff: true`. Set that only where the prohibition does not apply and the people
  concerned opted in.
- **No frame of a non-analysed participant reaches the model, and none is kept.**
- **What is inferred:**
  - per analysed participant and segment: expression, engagement, attention, body-language cues
    (posture, gestures, nods), and their reaction to what others said, also while listening;
  - an emotion label with a confidence, naming the signals (voice, face, body, words) that
    support it;
  - whether those signals disagree (reported, never averaged).
- **What is never inferred or kept:**
  - identity: the model sees only pseudonyms (`P1`, `P2`);
  - age, gender, ethnicity, health, religion, political or sexual orientation: keys like these are
    stripped from any answer, and `removedKeys` lists what was removed;
  - frames: they stay in memory until analysed and are never stored;
  - video recordings: the recordings are audio only, one per participant, sealed with envelope
    encryption and referenced by hash.
- **Advisory only.** Analysis is for people to review, never an automated decision about anyone.
  The stores carry the `personal` data class, so erasure and retention apply. The recordings they
  reference are covered too.

## Cost

Video sampling is the cost control, set on the `session-start` channel:

- `video.fps` (1 by default);
- `video.extra-frames`: one frame of everyone at turn boundaries and flagged moments (an
  interruption, raised energy, a long silence).

On `call-analysis`, `segment-seconds` and `max-frames` bound what each model call receives.
`when: periodic` analyses each segment as it closes rather than after the meeting.

## Not in this example

- **Video recording.** LiveKit Egress to the same bucket needs Redis and an Egress container;
  it is a follow-up.
- **Phone participants** via LiveKit SIP.
- **A minutes summary.** `examples/meeting-assistant` shows the transcript-to-minutes report job.
