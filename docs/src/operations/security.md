# Security notes

- **The media token.** Every media and webhook request must carry `SUTRA_VOIP_MEDIA_TOKEN` as
  `?token=`; with no token configured, every media request is refused. It travels in the URL because
  a PBX's bridge client sends no custom headers, so it can appear in access logs: keep those logs
  private, rotate the token, and use TLS (`wss://`) between gateway and engine anywhere outside a
  private network. The worker uses the same token as a reference.
- **The call-control token.** `SUTRA_VOIP_RWI_TOKEN` gives control of calls on the PBX (originate,
  transfer, hang up). Use `wss://` over untrusted networks; add a private CA with
  `SUTRA_VOIP_RWI_CA_FILE`.
- **Secrets are references.** Model keys, LiveKit keys, store credentials and the recording KEK are
  `env:` or `file:` references. The development tokens and extension passwords in the examples are
  for local use only.
- **Toll fraud.** An internet-facing PBX is attacked for premium-rate calling. See
  [Moving to a real SIP trunk](../guides/real-sip-trunk.md#toll-fraud).
- **Recordings.** Sealed with envelope encryption by default; the key-encryption key is a secret
  to protect and back up, since losing it makes sealed recordings unreadable.
- **Personal data.** Callers' audio, numbers and transcripts are personal data. Tag stores, audit
  metadata and not payloads, and use sensitive variables so they are redacted.
- **PINs.** Do not keep PINs in the engine's data stores in production (see the
  [ivr-dtmf example](../examples/ivr-dtmf.md)); the `dtmf` operation never echoes digits.
- **Supply chain.** CI runs `cargo audit`, `cargo deny` and Trivy scans on source and images.
