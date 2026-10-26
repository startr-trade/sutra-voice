# Security notes

- **The media token.** With no `SUTRA_VOIP_MEDIA_TOKEN` configured, every media request is
  refused. A `/voip/stream` client (the WebRTC worker, any media-stream gateway) sends it as
  `Authorization: Bearer`; a token in that URL is refused. The PCM bridge never sees it: each
  bridge URL carries a token minted for that one call, short-lived (`SUTRA_VOIP_BRIDGE_TOKEN_TTL_SECONDS`,
  60 s) and good for one socket. Only the PBX's webhook and IVR-step URLs carry the media token as
  `?token=`, because a PBX's webhook client sends no headers: keep those access logs private,
  rotate the token, and use TLS (`wss://`, `https://`) between gateway and engine anywhere outside
  a private network. Tokens are redacted from the transport's logs. See
  [Media credentials](../guides/voip-transport.md#media-credentials).
- **The call-control token.** `SUTRA_VOIP_RWI_TOKEN` gives control of calls on the PBX (originate,
  transfer, hang up). Use `wss://` over untrusted networks; add a private CA with
  `SUTRA_VOIP_RWI_CA_FILE`.
- **Secrets are references.** Model keys, LiveKit keys, store credentials and the recording KEK are
  `env:` or `file:` references. The development tokens and extension passwords in the examples are
  for local use only.
- **Toll fraud.** An internet-facing PBX is attacked for premium-rate calling. `originate` is
  deny-all until `SUTRA_VOIP_ORIGINATE_ALLOW` is set, refuses premium-rate and international
  numbers unless allowed, rate-limits per destination and in total, and writes every refusal to
  the audit journal (`voip.ORIGINATE_REFUSED`, `voip.ORIGINATE_RATE_LIMITED`). See
  [Moving to a real SIP trunk](../guides/real-sip-trunk.md#toll-fraud).
- **Recordings and segments.** Sealed with envelope encryption by default; the key-encryption key
  is a secret to protect and back up, since losing it makes sealed recordings unreadable. A crash
  leaves the part in progress spooled on disk, sealed (`SUTRA_VOIP_RECORDING_SPOOL_DIR`): put the
  spool on a volume only the engine can read.
- **Personal data.** Callers' audio, numbers and transcripts are personal data. Tag stores, audit
  metadata and not payloads, and use sensitive variables so they are redacted.
- **PINs.** Do not keep PINs in the engine's data stores in production (see the
  [ivr-dtmf example](../examples/ivr-dtmf.md)); the `dtmf` operation never echoes digits.
- **Supply chain.** CI runs `cargo audit`, `cargo deny` and Trivy scans on source and images.
