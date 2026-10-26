# Limitations

Voice, meeting and video support is a **preview** in 0.6. This page says what that means.

## Verified and not verified

- **Automated:** the suites run against SIPp (as carrier, caller and phones), a stub model server,
  a local rustpbx, a local LiveKit server, MinIO and PostgreSQL in Docker.
- **Manual only, not verified live by the automated suites:** a real SIP carrier, real model
  providers (OpenAI, Gemini or others), real conference bridges, and real meeting platforms. The
  examples' live variants are provided and documented, but a run against them is a manual check.

## Product limits

- **One gateway per engine process.** Only rustpbx has a call-control adapter (checked against
  0.5.1). Asterisk and FreeSWITCH are planned.
- **Turn-based, not realtime speech-to-speech.** Media is not bridged straight between a call and a
  model's live API with the flow as control plane.
- **Turn detection needs continuous media.** A gateway that sends nothing during silence ends a turn
  late.
- **Video is not available through rustpbx**: its media bridge is audio only. Video comes through the
  WebRTC worker.
- **Worker:** no video recording (recordings are audio only; frames are analysed and dropped), no
  phone participants through LiveKit SIP, one room per worker.
- **Dial-in transcripts do not label speakers** (one mixed channel).
- **Recording is in memory** until the hang-up, bounded by `SUTRA_VOIP_RECORDING_MAX_SECONDS`.
- **Not a media server:** no conferencing, mixing or transcoding beyond PCM16 and G.711 handling.
- **Prompts in the IVR example are tones, not speech;** replace them with recordings.
- **Legal compliance is yours:** the software provides disclosure and restriction hooks, not a
  determination that your use is lawful.
