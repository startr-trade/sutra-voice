# Recording and consent

## How a recording is made

A listen-only call is recorded when its `originate` request comes from a process that declares
`<q:audit capture="media">`. The transport keeps the far end's audio (silence-filled where the
gateway sent nothing, so the recording keeps the call's clock, with the transport's own prompts
mixed in) and, at the hang-up, writes it once as `audio/wav` to the configured object store.

- **Store.** `SUTRA_VOIP_RECORDING_STORE` is `s3://<bucket>[/<prefix>]` for any S3-compatible store.
  A call to be recorded with no store configured is refused (`SUTRA.VOIP.RECORDING_UNAVAILABLE`).
- **Sealed by default.** Each recording gets a fresh data key, wrapped under
  `SUTRA_VOIP_RECORDING_KEK`. Set the encryption to `none` only when the store encrypts at rest.
- **Referenced, never embedded.** `call-hangup` and the process's audit events carry a reference:
  URI, `sha256` of the stored object, size, duration, format, encryption scheme and key id. The
  hash lets the object's integrity be checked without the key.
- **Bounded.** Recording is held in memory until the hang-up (about 58 MB an hour at 8 kHz mono) and
  capped by `SUTRA_VOIP_RECORDING_MAX_SECONDS` (default 7200).
- **Failure is visible.** A store that cannot be written yields `recording: {status: "failed", ...}`.

The full key list is in [Configuration](../operations/configuration.md).

## Consent

Recording and analysis rules differ by jurisdiction: some require every party's consent, some an
announcement, some a way to opt out. The software cannot decide which applies; the flow carries the
announcement, and the platform makes it provable.

- **Say it before anything is recorded.** Play the announcement as the first step, with
  `consent: true` on the `play` so the transport notes it. The recording reference then states
  `consentAnnounced` and `consentAnnouncedAtMs`.
- **Meeting bots are visible.** The WebRTC worker joins under a display name that says it records
  (default `Meeting assistant (recording)`), and the example flow announces recording and analysis
  in the room.
- **Personal data.** Mark stores holding interaction records as personal, audit metadata and not
  payloads, treat the caller's turns as sensitive variables so they are redacted wherever they would
  be logged, and mark the caller number as a subject key so erasure requests find it.
- **A menu that does not record says so by not recording.** The `ivr-dtmf` example neither records
  nor analyses; if you add either, update the welcome prompt first.

This page is engineering guidance, not legal advice.
