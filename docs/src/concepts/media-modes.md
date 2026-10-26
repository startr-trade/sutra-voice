# Mixed and per-participant media

| | Mixed | Per-participant |
|---|---|---|
| Source | A PBX conference, dialled in over SIP | A LiveKit room through the WebRTC worker |
| What the engine receives | One stream carrying everyone | One tagged stream per participant, plus sampled camera frames |
| Transcript speakers | `unknown`, unless the provider labels them | Per participant, from that participant's own track |
| Per-participant analysis | Not possible | Yes, for analysed roles only |
| Recording | One recording of the mix | One recording per participant, audio only |

A conference dial-in is one mixed channel, so a live transcription cannot say who spoke. Providers
such as OpenAI Realtime and Gemini Live label no speakers, so the transcript marks the speaker as
`unknown` with the source `none`; attribute speakers afterwards, for example with a model over the
transcript, and say that they were inferred.

Per-participant media is where emotion and engagement analysis, if you use it, is possible at all,
and also where the compliance obligations bite hardest. See
[Emotion analytics and compliance](../guides/emotion-analytics.md).

Sutra Voice does not mix: a mixed stream is whatever the PBX produced. Hosted per-participant
audio on PBXs is on the [Roadmap](../roadmap.md).
