# voice-assistant — phone calls handled by flows

A clinic front desk on the phone, end to end:

- **An agentic responder on a number.** A call to `+15551230000` is greeted, and every utterance
  of the caller goes — as audio — to an audio-native model, whose spoken answer is played back.
  Speaking over an answer interrupts it (barge-in). Keys work when speech does not: `0` reaches an
  agent, `#` ends the call. The model hands the caller to an agent when asked. The hang-up frees
  the trunk line at once.
- **Post-call analytics.** After the call, the customer's voice is analysed — emotion and
  sentiment per turn and over the call, from a model that hears the audio itself — alongside the
  acoustic metrics the transport measured (talk time, silence, dead air, interruptions, pitch,
  energy). A decision table works out first-contact resolution and repeat contact (same caller
  within 7 days, found by caller number in the data store), and the interaction is stored.
- **Appointment-reminder calls.** Hand over an appointment and the engine calls the patient
  through the same trunk, reminds them, hears their answer and replies — or, when the line is
  busy or nobody answers, tries again after a pause (a retry loop with a counter; two attempts
  in all).

It runs locally with no key and no phone line: [rustpbx](https://github.com/restsend/rustpbx) is
the media gateway, SIPp plays the carrier (and a caller), and a stub stands in for the models.

## What is in here

```
inbound/                         the voice-assistant module: the number, the turns, the analytics
  shared/bpmn/inbound-call.bpmn    the CALL, one instance: greet, the conversation loop (each utterance to
                                   the model, audio in and out; keys: 0 = agent, # = goodbye), post-call analytics
  shared/rules/contact-outcome.dmn first-contact resolution / repeat contact — your definitions, edit them
  shared/datastores.yaml           the `interactions` store (interaction record, caller index)
  variants/inbound-stub|live/      channels.yaml (+ package.yaml): stub models, or real providers
reminders/                       the voice-reminders module
  shared/bpmn/reminder-call.bpmn   dial on the pooled channel, wait for the outcome, talk — or loop back and retry
  variants/reminders-stub|live/
deploy/compose.yaml              engine + PostgreSQL + rustpbx + SIPp + stub
deploy/rustpbx/config.toml       the PBX: trunk, the number's route, call control, event tap
deploy/sipp/                     the stand-in carrier (scenarios + a synthesised utterance)
```

The models are the shared stub in `../model-stub`.

**The conversation is a loop.** A call is one instance from the greeting to the analytics. After
every answer it goes back to the same wait — "in the call": the caller speaks, presses a key,
goes quiet for two minutes, or hangs up — and each time round that wait is a new one, with its
own timer. The conversation so far (what was said, and each utterance's audio and acoustic
features for the analysis) lives in the instance's own variables; nothing is passed between
turns through the data store. The transport delivers a call's messages strictly in order, one at
a time, and holds the next while the instance is still answering, so turns never overlap and an
utterance spoken over an answer is answered next. The reminder's retry is the same kind of loop:
dial, wait for the outcome, and on busy / no answer go round again while `attempts <
maxAttempts`.

## Run it with the stub

You need the `sutra` CLI and Docker. Until the 0.6.0 image is published, build it from this tree
(`make image`) and set `SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev`.

```bash
sutra package examples/voice-assistant/inbound/variants/inbound-stub \
    --overlay examples/voice-assistant/inbound/shared --out examples/voice-assistant/deploy/deployments
sutra package examples/voice-assistant/reminders/variants/reminders-stub \
    --overlay examples/voice-assistant/reminders/shared --out examples/voice-assistant/deploy/deployments
docker compose -f examples/voice-assistant/deploy/compose.yaml up -d --build
```

**Call the assistant.** SIPp dials the number, waits for the greeting, says one thing (a
synthesised 1.5 s utterance), listens, and hangs up:

```bash
docker compose -f examples/voice-assistant/deploy/compose.yaml run --rm caller
```

Then read what the call left behind — the interaction record and the caller's contact index:

```bash
docker compose -f examples/voice-assistant/deploy/compose.yaml exec engine-db \
  psql -U postgres -d sutra -c \
  "select store_key, store_value from data_store where store_name = 'interactions'"
```

The record holds the outcome (`first-contact-resolution`), the analysis of the customer's voice,
the call's acoustic metrics, and each turn's features. Call again from the same number: the
second call is `resolved-on-repeat` (a repeat contact), and the first call's
`firstContactResolution` turns `false`.

**Send a reminder.** The engine dials the carrier, which answers and says one thing:

```bash
curl -X POST -H 'X-Api-Key: dev-only-voice-key' -H 'Content-Type: application/json' \
  -d '{"appointmentId":"A-1","name":"Sam","phone":"+15550001111","when":"Tuesday at 10:00"}' \
  http://127.0.0.1:$(docker compose -f examples/voice-assistant/deploy/compose.yaml port engine 8080 | cut -d: -f2)/channels/reminder
```

The outcome lands in the `reminders` store under `A-1`.

**The trunk's lines.** `compose.yaml` declares the `trunk-main` capacity pool with 4 lines, one
reserved for inbound. Start the engine with `TRUNK_LINES=2` and send two reminders at once: the
second waits in the outbox until the first call ends, then dials. A call to the number when every
line is taken is refused busy.

Stop it with `docker compose -f examples/voice-assistant/deploy/compose.yaml down`.

## Switch to a real trunk and real models

1. **The models.** Package the `*-live` variants instead of the `*-stub` ones and export
   `OPENAI_API_KEY` (the responder: an OpenAI audio chat model) and `GEMINI_API_KEY` (the
   analysis: Gemini's native API, with the OpenAI audio model as fallback) before `up`.
2. **The trunk** (`deploy/rustpbx/config.toml`, section `[proxy.trunks.carrier]`): set `dest` to
   your carrier's SIP proxy, `username` / `password` to your credentials, `inbound_hosts` to the
   carrier's signalling addresses only (rustpbx recognises a trunk by the source address), and
   `did_numbers` to your number. Keep `codec` — originated calls need it.
3. **The number.** Change the route's `"to.user"` match, and the `call-start` channel's
   `bind: "number:…"` in `inbound/variants/*/channels.yaml`.
4. **The network.** Set `external_ip` to the address the carrier sends media to, publish SIP and
   exactly the RTP range of `config.toml` on the `pbx` service (the commented `ports:` line), and
   restrict `acl_rules`.
5. **The secrets.** Replace the dev-only RWI and media tokens (in `config.toml` and the engine's
   environment), use `wss://` for the control socket if the PBX is reached over an untrusted
   network (`SUTRA_VOIP_RWI_CA_FILE` for a private CA), and put the engine behind TLS for the
   media WebSocket.
6. **The agents.** Point the transfer target (`sip:2001@pbx` in `inbound-call.bpmn`) at your
   agents' extension or queue.
7. **The trunk size.** Set `SUTRA_CAPACITY_POOLS_TRUNK_MAIN_SLOTS` to your trunk's concurrent-call
   limit and the inbound reserve to what you never want outbound calls to take.

A real trunk stays a manual check: the engine's automated tests run against SIPp.

## Consent, announcement, and whose voice is analysed

- **Say it up front.** The greeting tells the caller that the call is recorded and analysed, and
  the reminder states that the call is recorded. Recording and analysis rules differ by
  jurisdiction — check yours, and adjust the greeting (in `inbound-call.bpmn`) to what you must
  say, including how to opt out.
- **Customers only, by default.** The analysis covers the customer's voice
  (`analyzedParties: ["customer"]`). An agent's leg never reaches the engine — a transfer hands the
  call to the PBX — and **agent-side emotion analysis is opt-in only**. Do not add it lightly: in
  the EU, the AI Act prohibits inferring the emotions of people at work, which includes
  contact-centre agents; analysing customers is allowed with transparency obligations. If you do
  enable it elsewhere, make it a deliberate per-deployment switch and document why it is lawful
  there.
- **No identification, nothing sensitive.** The analysis prompt asks for emotion and sentiment
  only, never who the person is or any sensitive attribute.
- **Advisory, not a decision.** The analysis is data for people and one input of the outcome
  table — never an automated decision about the caller.
- **Personal data.** The interaction record and the conversation (it contains the caller's
  audio) are personal data: the `interactions` store is tagged `personal`, the processes audit
  metadata only (not payloads), the caller's turns are a sensitive variable (redacted wherever
  they would be logged), and the caller number is marked sensitive and a subject key, so erasure
  and retention can find it.

## Tests

The tier-2 suite (`rust/crates/sutra-voice-conformance/tests/all/tc_voice_assistant.rs`) runs
this example on a Docker network — the engine image, rustpbx pinned by digest, SIPp, the stub,
PostgreSQL — and proves an inbound call (greeting and answer received by the caller as RTP, one
instance looping back to its in-call wait and leaving it at the hang-up, the line held and then
freed, the analysis and outcome stored, a repeat call recognised), a reminder call with one
turn, and an originate waiting while the trunk is full.

`examples/ivr-dtmf` is the same stack without a model: a DTMF phone menu with recorded prompts,
extensions and PINs.
