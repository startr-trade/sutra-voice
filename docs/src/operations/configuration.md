# Configuration

Configuration is environment variables, and every secret is a reference (`env:NAME`,
`file:/path`, `secret:...`), never a literal. A literal key is refused.

## Engine (voip transport)

The complete table with defaults is in [The voip transport](../guides/voip-transport.md#engine-configuration).
In groups:

| Group | Variables |
|---|---|
| Gateway | `SUTRA_VOIP_GATEWAY` (`generic` or `rustpbx`), `SUTRA_VOIP_TRUNK` |
| Call control (rustpbx) | `SUTRA_VOIP_RWI_URL`, `SUTRA_VOIP_RWI_TOKEN`, `SUTRA_VOIP_RWI_CA_FILE` |
| Media | `SUTRA_VOIP_PUBLIC_MEDIA_URL`, `SUTRA_VOIP_MEDIA_TOKEN`, `SUTRA_VOIP_BRIDGE_RATE` |
| Prompts | `SUTRA_VOIP_PROMPTS_DIR` |
| Recording | `SUTRA_VOIP_RECORDING_STORE`, `_ENDPOINT`, `_REGION`, `_ACCESS_KEY`, `_SECRET_KEY`, `_CREATE_BUCKET`, `_ENCRYPTION`, `_KEK`, `_KEY_ID`, `_MAX_SECONDS` |

Channel-level settings (turn detection, `mode`, `record`, `transcribe`, `analyze`, `video.*`,
`phrases`, `pool`, `session-end`) are keys on the package's channels, described in the guides.

## Engine (general)

`SUTRA_DEPLOYMENTS_DIR`, `SUTRA_HTTP_PORT`, `SUTRA_DATASOURCE_*` and the capacity-pool settings
(`SUTRA_CAPACITY_POOLS`, `SUTRA_CAPACITY_POOLS_<NAME>_SLOTS`, ...) belong to the engine. See the
[Sutra configuration reference](https://sutra.startr.trade/operating/configuration.html).

## WebRTC worker

`LIVEKIT_URL`, `SUTRA_WORKER_ROOM`, `SUTRA_WORKER_API_KEY`, `SUTRA_WORKER_API_SECRET`,
`SUTRA_WORKER_MEDIA_TOKEN`, `SUTRA_WORKER_ENGINE_URL`, `SUTRA_WORKER_CALLED`,
`SUTRA_WORKER_IDENTITY`, `SUTRA_WORKER_NAME`, `SUTRA_WORKER_AUDIO_RATE`,
`SUTRA_WORKER_ROLE_ATTRIBUTE`, `SUTRA_WORKER_EMPTY_GRACE_MS`, `SUTRA_WORKER_ONCE`. Defaults are in
[The WebRTC worker](../guides/webrtc-worker.md#running-the-worker).

## Diagnostics

Misconfiguration surfaces as `SUTRA.VOIP.*` diagnostic codes (for example `CONFIG_INVALID`,
`CHANNEL_INVALID`, `BIND_DUPLICATE`, `MODEL_CHANNEL_INVALID`, `RECORDING_UNAVAILABLE`); the list is
at the end of the [voip transport guide](../guides/voip-transport.md#diagnostics).
