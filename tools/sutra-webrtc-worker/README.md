# sutra-webrtc-worker

The WebRTC media worker: a bot that joins a [LiveKit](https://livekit.io) room as a visible
participant, subscribes to every other participant's audio and video, and forwards them to a
Sutra engine over the voip transport's media-stream WebSocket — one session per meeting, each
participant announced with a session index, a display label and its declared role (never its
identity), audio as PCM16 mono, video as JPEG frames sampled at the rate the engine asks for.

It is a separate binary in its own Cargo workspace because the LiveKit SDK links libwebrtc; the
engine never does. See the engine's guide (the voip transport's multi-party sessions) for the
protocol and the flow side.

## Build and check

```bash
make worker-image   # sutra-webrtc-worker:dev — both binaries
make worker-lint    # fmt + clippy -D warnings (in Docker), cargo deny + audit
make worker-test    # unit tests (in Docker)
make worker-scan    # Trivy over the image
```

The builder needs clang 21 or newer (libwebrtc's own libc++ requires it); the Dockerfile
installs it, so the host needs only Docker.

## Run

| Variable | Meaning | Default |
|---|---|---|
| `LIVEKIT_URL` | the LiveKit server | `ws://livekit:7880` |
| `SUTRA_WORKER_ROOM` | the room to join | required |
| `SUTRA_WORKER_API_KEY` / `SUTRA_WORKER_API_SECRET` | LiveKit API key and secret — **references** (`env:NAME`, `file:/path`); a literal is refused | required |
| `SUTRA_WORKER_MEDIA_TOKEN` | the engine's media token — a reference | required |
| `SUTRA_WORKER_ENGINE_URL` | the engine's media-stream endpoint | `ws://engine:8080/voip/stream` |
| `SUTRA_WORKER_CALLED` | the number the session presents as `to` (what the engine's `call-start` channel binds) | required |
| `SUTRA_WORKER_IDENTITY` / `SUTRA_WORKER_NAME` | the bot's identity and display name | `sutra-meeting-bot` / `Meeting assistant (recording)` |
| `SUTRA_WORKER_AUDIO_RATE` | audio rate sent to the engine (8000, 16000, 24000, 48000) | `16000` |
| `SUTRA_WORKER_ROLE_ATTRIBUTE` | the participant attribute holding a participant's role | `role` |
| `SUTRA_WORKER_EMPTY_GRACE_MS` | how long the room may be empty before the session ends | `3000` |
| `SUTRA_WORKER_ONCE` | exit after one session instead of waiting for the next meeting | `false` |

Roles are declared, not inferred: whoever issues a participant's access token sets the role
attribute (`external`, `staff`, …). A participant without one is announced with `role: null`.

## The scripted participant

`sutra-webrtc-participant` (in the same image) is a test and demo tool, not a client: it joins a
room and publishes a synthetic microphone (speech-like bursts on a talk script) and a synthetic
camera (a solid colour with a moving bar).

```bash
docker run --rm --entrypoint sutra-webrtc-participant \
  -e LIVEKIT_URL=ws://livekit:7880 -e SUTRA_WORKER_API_KEY=env:LK_KEY -e SUTRA_WORKER_API_SECRET=env:LK_SECRET \
  -e LK_KEY=devkey -e LK_SECRET=secret sutra-webrtc-worker:dev \
  --room standup --identity p1 --name "Alex" --role staff --talk "1-3,6.5-8.5" --duration 12
```
