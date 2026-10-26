# Meeting assistant: join, listen, transcribe, report

A bot that dials into a conference by its SIP dial-in number, enters the meeting PIN, announces
that the call is recorded and transcribed, and then only listens. The `voip` transport records
the meeting and transcribes it live; the flow hears only sparse events — the dial's outcome, a
phrase being heard, the end of the meeting. After the call the flow reads the transcript back,
asks a model for the minutes, applies the analysis policy, renders a report and stores and sends
it, with a reference to the recording.

```
join ─► dial (voip://originate, mode: listen) ─► connected? ─► PIN (voip://dtmf) ─► consent prompt
     ─► IN THE MEETING ◄─┐  call-keyword: note it ─┘
                          call-hangup / 4 h: leave
     ─► read the transcript ─► report job (JSON) ─► policy ─► render ─► store + send
```

Audio never passes through the flow. The process audits at `<q:audit capture="media">`, which
marks every request it sends; the transport records a marked call to the object store, sealed
(envelope encryption), and the flow — and the audit trail — get only the recording's
**reference**: its URI, the SHA-256 of the stored object, its duration and format, and whether the
consent announcement was played.

| Path | What |
|---|---|
| `shared/bpmn/meeting-call.bpmn` | the meeting: join, listen, report |
| `shared/bpmn/transcript-segment.bpmn` | stores each transcript segment as it completes |
| `shared/bpmn/scheduled-join.bpmn` | a standing meeting joined on schedule (a weekly timer start) |
| `shared/datastores.yaml`, `shared/schemas/meeting/`, `shared/migrations/` | `transcripts` (a projected store: one typed row per segment) and `meetings` (reports, standing meetings) |
| `shared/templates/meeting-report.hbs` | the report |
| `variants/meeting-stub/`, `variants/meeting-live/` | the channels: the stub model, or real providers |
| `prompts/` | the consent announcement (a stand-in tone pattern; see below) |
| `deploy/` | compose stack: engine, PostgreSQL, MinIO, rustpbx, SIPp as the conference, the stub model |

## Run it with the stub (no keys, no network)

```bash
sutra package examples/meeting-assistant/variants/meeting-stub \
    --overlay examples/meeting-assistant/shared --out examples/meeting-assistant/deploy/deployments
docker compose -f examples/meeting-assistant/deploy/compose.yaml up -d --build
PORT=$(docker compose -f examples/meeting-assistant/deploy/compose.yaml port engine 8080 | cut -d: -f2)

curl -X POST -H 'X-Api-Key: dev-only-meeting-key' -H 'Content-Type: application/json' \
     -d '{"meetingId":"M-1","title":"Launch plan review","dialIn":"+15550142000","pin":"4321",
          "participants":[{"name":"Alex Chen","role":"internal"},{"name":"Sam Rivera","role":"external"}]}' \
     http://127.0.0.1:$PORT/channels/join
```

The stand-in conference (SIPp) answers, waits while the bot enters the PIN and announces itself,
plays a synthesised two-voice conversation (about 11 s) and hangs up. About 25 seconds after the
request:

```bash
# the transcript, one row per segment
docker compose -f examples/meeting-assistant/deploy/compose.yaml exec engine-db \
  psql -U postgres -d sutra -c "SELECT seq, speaker, start_ms, end_ms, text FROM transcripts ORDER BY seq"
# the report (summary, decisions, action items, speakers, the rendered text, the recording's reference)
docker compose -f examples/meeting-assistant/deploy/compose.yaml exec engine-db \
  psql -U postgres -d sutra -tAc "SELECT store_value FROM data_store WHERE store_key = 'report:M-1'"
# the audited reference
docker compose -f examples/meeting-assistant/deploy/compose.yaml exec engine-db \
  psql -U postgres -d sutra -tAc "SELECT payload_json FROM audit_event WHERE payload_json LIKE '%\"media\"%' LIMIT 1"
```

The recording is in the `meeting-recordings` bucket of the MinIO container under
`calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed`. Its SHA-256 is the audited one
(`sha256sum` over the object). It is sealed: a header names the key id and carries the wrapped
data key, and only the key-encryption key (`SUTRA_VOIP_RECORDING_KEK`) opens it — with
`sutra_objectstore::sealed::open` from Rust.

The tier-2 test `tc_meeting_assistant_joins_records_transcribes_and_reports` (sutra-voice-conformance)
runs this exact stack and checks each of those, plus the PIN as RFC 2833 events at the conference
and the trunk line released at the hang-up.

## A real conference

- **The dial-in.** Most conference services take calls on a PSTN dial-in number with a numeric
  meeting PIN. Point the `conference` trunk in `deploy/rustpbx/config.toml` at your SIP carrier
  (`dest`, `username`, `password`), and post the service's dial-in number as `dialIn` (a bare
  number is dialled through `SUTRA_VOIP_TRUNK`) and the meeting's PIN as `pin`. If the service
  wants `#` after the PIN, the flow sends it; if it wants a participant PIN or a menu choice
  first, change `PrepPin` (the digits) or send a second `voip://dtmf`. Publish rustpbx's RTP range
  (UDP) and set `external_ip` when the carrier is outside the Docker network.
- **The announcement.** `prompts/consent.wav` is a stand-in tone pattern (no offline TTS is
  assumed). Record the wording your counsel approves — for example: "This call is recorded and
  transcribed by an automated assistant, and a summary will be shared with the participants." —
  as a WAV of the same name (any rate; PCM16, µ-law or A-law).
- **Standing meetings.** `scheduled-join.bpmn` fires every Monday at 09:00 UTC and joins the
  meeting stored as `schedule:weekly-review` in the `meetings` store (the same JSON a caller would
  POST; the date is appended to its `meetingId`). Nothing stored, nothing dialled. Change the
  `timeCycle` for another cadence; one process per standing meeting.
- **Recordings.** Any S3-compatible store: set `SUTRA_VOIP_RECORDING_STORE` to
  `s3://<bucket>/<prefix>`, drop `SUTRA_VOIP_RECORDING_ENDPOINT` for AWS itself, and give the
  credentials and the KEK as `secret:` / `vault:` references. `SUTRA_VOIP_RECORDING_ENCRYPTION=none`
  leaves sealing to a store that encrypts at rest itself. The whole call is held in memory until
  the hang-up (8 kHz mono, about 58 MB an hour; `SUTRA_VOIP_RECORDING_MAX_SECONDS` bounds it).

## Live transcription: which providers

The `call-transcript` channel opens a **transcription-only live session** for the call, through
the `meeting-transcription` llm channel (`transcribe.channel`). The model is configured there,
like every model call of the package: ordered candidates, `require: [realtime]` (only a candidate
with live sessions serves it), fallback on rate limits, outages and timeouts, keys as references.
Add a second candidate to fall back to (the live variant shows Gemini Live, commented out).

| Provider | candidate `provider` | What it does |
|---|---|---|
| OpenAI Realtime | `openai` | A transcription session (`transcription-model`, e.g. `gpt-4o-transcribe`); speech start and end come with audio offsets. Verified against the stub only — check the session shape against the current API reference before relying on it. |
| Gemini Live | `gemini` | Input transcription on a Live session. The Live API has no listen-only mode, so the model is told to stay silent and anything it says is ignored; segment timing is the transport's own clock. |

Neither labels **speakers**, and a conference dial-in delivers one mixed audio channel, so every
segment is `speaker: "unknown"`. The report model attributes the transcript to the participants
from what was said (names, the participant list), and the report says the attribution is
inferred. Exact attribution needs per-participant audio (a WebRTC join with one track per
participant), which the dial-in path cannot give.

The report itself is a **background job** (`mode: job` on `report-model`): a long transcript holds
no connection while the model writes the minutes. The stub answers it; the live variant uses
OpenAI's background responses.

## Consent, recording law and emotion analysis

- **Consent.** Recording and transcription consent rules differ by jurisdiction (one-party,
  all-party, or notice-based). The bot announces itself before it listens, and the recording
  states whether the announcement was played to the end and where (`consentAnnounced`,
  `consentAnnouncedAtMs`). The recording starts when the media connects, so the announcement is
  in it; if your jurisdiction requires consent before any recording, get it out of band (the
  meeting invitation) or do not record (`<q:audit capture="metadata">`: no recording, transcript
  still produced). This example is not legal advice.
- **Emotion analysis is regulated.** In the EU (AI Act) inferring the emotions of people **at
  work** is prohibited — that includes your own staff in an internal meeting. So the policy is
  per party: **external** participants are analysed; **internal** participants only when the
  deployment allows it (`internalAnalysisAllowed` in `meeting-call.bpmn`, `false` as shipped)
  AND the request opts them in (`analysis.internalOptIn`). Do not turn the switch on for an EU
  workplace. The policy is applied twice: the prompt asks for no emotion of a non-analysed party,
  and whatever the model returns for one anyway is dropped before the report is written.
- **Advisory only.** Sentiment and emotion in the report are a model's inference for the people
  reading it — never an automated decision about anyone. The report model sees the transcript and
  the call's acoustic metrics, not the audio.
- **Personal data.** The join request, the transcript, the report and the recording are personal
  data. The processes tag their messages with GDPR data classes, the stores are declared
  `personal`, the PIN is a `sensitive` variable (persisted for the call, redacted in logs and
  audit, never in the report), and recordings live under one prefix per module, so erasure and
  retention can reach them through the audited references.

## What the stub stands for

`examples/model-stub/model-stub.py` answers the transcription session over a WebSocket
(an energy detector cuts the audio at 500 ms pauses; each stretch is answered with the next line
of a fixed four-line meeting) and the report job with a fixed JSON report. No prompt is read for
meaning, and nothing is sent anywhere.
