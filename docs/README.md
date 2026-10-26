# sutra-voice documentation

| Page | What it covers |
|---|---|
| [Phone and VoIP calls: the `voip` transport](voip-transport.md) | Channels (`call-start`, `call-turn`, `call-dtmf`, `call-progress`, `call-hangup`, `call-transcript`, `call-keyword`, `call-participant`, `call-analysis`), outbound `voip://` operations, the media-stream protocol, gateways (generic, rustpbx), listen-only calls and recordings, meeting sessions, segment analysis, `SUTRA_VOIP_*` settings and `SUTRA.VOIP.*` diagnostics. |
| [Video meetings: the WebRTC worker](webrtc-worker.md) | The LiveKit bot, its configuration, the test participant, building and scanning its image. |

Everything engine-side is in the Sutra book:

- [Channels and transports](https://sutra.startr.trade/building/channels.html)
- [Calling a language model](https://sutra.startr.trade/building/llm-channels.html) — the `llm`
  channels the transport's transcription and analysis go through
- [Deployment packages](https://sutra.startr.trade/building/deployment-packages.html)
- [Wait states and human tasks](https://sutra.startr.trade/building/wait-states.html)
- [Configuration reference](https://sutra.startr.trade/operating/configuration.html) — capacity
  pools and the engine-wide settings
- [Installing the `sutra` CLI](https://sutra.startr.trade/getting-started/installation.html)
