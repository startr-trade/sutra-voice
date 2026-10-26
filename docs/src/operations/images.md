# Images

| Image | Built by | What it is |
|---|---|---|
| `sutra-voice-engine:dev` | `make image` | The Sutra engine's default bundle plus the `voip` transport. The public engine image does not include `voip`. |
| `sutra-webrtc-worker:dev` | `make worker-image` | The LiveKit bot and `sutra-webrtc-participant`, a scripted participant for tests and demos. Built in Docker; the first build fetches libwebrtc. |

Both tags are mutable development tags. Rebuild from the commit you are testing before trusting a
Docker-level result, since a stale tag silently tests an older binary. The examples read the engine
image from `SUTRA_ENGINE_IMAGE`; set it to `sutra-voice-engine:dev` when working from a clone.

The engine image runs exactly like the Sutra engine's (`SUTRA_DEPLOYMENTS_DIR`, `SUTRA_HTTP_PORT`,
`SUTRA_DATASOURCE_*`) plus the `SUTRA_VOIP_*` settings. Pin third-party images such as rustpbx,
which publishes only a moving `latest` tag, by digest in anything beyond a local trial.
