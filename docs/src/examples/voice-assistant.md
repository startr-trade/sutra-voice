# voice-assistant

A clinic front desk on the phone. A call to the example number is greeted, and each utterance goes
as audio to an audio-native model whose spoken answer is played back, with barge-in and key
fallbacks (`0` for an agent, `#` to end). The call is recorded once the greeting, which says so,
has played to the end (`record: after-consent`). After the call the customer's voice is analysed
for emotion and sentiment alongside the transport's acoustic metrics, a decision table works out
first-contact resolution and repeat contact, and the interaction is stored. A second flow places
appointment-reminder calls through the same capacity pool, with a retry loop.

It runs with the stub model server: no key and no phone line. A set of `*-live` variants swaps in
real models. The README also has the checklist for a real trunk and a consent section that applies
to anything that records or analyses callers.

## Walkthrough: a call, a repeat call, and a reminder

**Prerequisites.** Docker with Compose v2, `curl` and `python3`, and a checkout of this
repository with its submodule. Until 0.6.0 is published, build the engine image and the `sutra`
CLI from the checkout (the CLI comes from the engine submodule; the first build takes several
minutes):

```bash
git submodule update --init
make image
cargo build --manifest-path engine/rust/Cargo.toml --profile release-it -p sutra-cli --target-dir rust/target
export PATH="$PWD/rust/target/release-it:$PATH"
export SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev
```

**1. Package both modules (the inbound assistant and the reminders) and start the stack**
(engine, PostgreSQL, rustpbx, SIPp as the carrier, the stub model):

```bash
C=examples/voice-assistant/deploy/compose.yaml
sutra package examples/voice-assistant/inbound/variants/inbound-stub \
    --overlay examples/voice-assistant/inbound/shared --out examples/voice-assistant/deploy/deployments | grep '^packaged'
sutra package examples/voice-assistant/reminders/variants/reminders-stub \
    --overlay examples/voice-assistant/reminders/shared --out examples/voice-assistant/deploy/deployments | grep '^packaged'
docker compose -f $C --progress quiet up -d --build
until PORT=$(docker compose -f $C port engine 8080 2>/dev/null | cut -d: -f2) &&
      [ "$(curl -sf "http://127.0.0.1:$PORT/sutra/deployments" | grep -o '"phase":"Active"' | wc -l)" = 2 ]; do sleep 1; done
echo "the engine is on port $PORT"
```

```text
packaged examples/voice-assistant/deploy/deployments/inbound-stub.sutra (deploymentId dep-7395ba4564bbcbdc4c76649b)
packaged examples/voice-assistant/deploy/deployments/reminders-stub.sutra (deploymentId dep-9faf75d6967125d67f28a186)
the engine is on port 33011
```

**2. Call the assistant.** SIPp dials the number from `+15557654321`, waits for the greeting,
says one thing (a synthesised 1.5 s utterance), listens to the answer and hangs up:

```bash
docker compose -f $C --progress quiet run --rm caller >/dev/null 2>&1; echo "the caller exited with $?"
db() { docker compose -f $C exec -T engine-db psql -U postgres -d sutra -tAc "$1"; }
until [ "$(db "select count(*) from data_store where store_name = 'interactions' and store_key like 'call:%'" 2>/dev/null)" = 1 ]; do sleep 1; done
db "select store_value from data_store where store_name = 'interactions' and store_key like 'call:%'" | python3 -c '
import json, sys
r = json.load(sys.stdin)
print("outcome:", r["outcome"], "firstContactResolution:", r["firstContactResolution"], "repeatContact:", r["repeatContact"])
print("analysis:", r["analysis"]["customer"]["overall"], [t["emotion"] for t in r["analysis"]["customer"]["trajectory"]], "audioPreserved:", r["audioPreserved"])
print("acoustic:", {k: r["acoustic"][k] for k in ["callerTalkMs", "agentTalkMs", "turns", "interruptions"]})
rec = r["recording"]
print("recording:", rec["kind"], rec["format"], "startMs=%s consentAnnouncedAtMs=%s complete=%s" % (rec["startMs"], rec["consentAnnouncedAtMs"], rec["complete"]))'
```

```text
the caller exited with 0
outcome: first-contact-resolution firstContactResolution: True repeatContact: False
analysis: neutral ['concerned', 'calm'] audioPreserved: True
acoustic: {'callerTalkMs': 1380, 'agentTalkMs': 780, 'turns': 1, 'interruptions': 0}
recording: media audio/wav startMs=805 consentAnnouncedAtMs=-805 complete=True
```

The model heard the voice itself (`audioPreserved: true`). The recording started when the
greeting ended (`startMs`), and the greeting itself began before it (`consentAnnouncedAtMs` is
negative): nothing was recorded before the caller had heard that the call is recorded.

**3. Call again from the same number.** The decision table recognises a repeat contact, and the
first call no longer counts as resolved at first contact:

```bash
docker compose -f $C --progress quiet run --rm caller >/dev/null 2>&1; echo "the caller exited with $?"
until [ "$(db "select count(*) from data_store where store_name = 'interactions' and store_key like 'call:%'")" = 2 ]; do sleep 1; done
sleep 2
db "select store_value from data_store where store_name = 'interactions' and store_key like 'call:%'" | python3 -c '
import json, sys
for line in sys.stdin:
    r = json.loads(line)
    print(r["startedAt"][:19], r["outcome"], "firstContactResolution=%s repeatContact=%s" % (r["firstContactResolution"], r["repeatContact"]))' | sort
db "select store_value from data_store where store_key = 'caller:+15557654321'"
```

```text
the caller exited with 0
2026-10-26T07:34:21 first-contact-resolution firstContactResolution=False repeatContact=False
2026-10-26T07:34:34 resolved-on-repeat firstContactResolution=False repeatContact=True
{"contacts":2,"lastAt":"2026-10-26T07:34:46.835616706Z","lastCallId":"818dc3f32fc740d7a3773b9db28efcd9","lastResolved":true}
```

**4. Send a reminder.** The engine dials the carrier (through the same pool of trunk lines),
which answers and says one thing; the outcome lands in the `reminders` store:

```bash
curl -s -w "HTTP %{http_code}\n" -X POST "http://127.0.0.1:$PORT/channels/reminder" \
  -H 'X-Api-Key: dev-only-voice-key' -H 'Content-Type: application/json' \
  -d '{"appointmentId":"A-1","name":"Sam","phone":"+15550001111","when":"Tuesday at 10:00"}'
until [ -n "$(db "select 1 from data_store where store_name = 'reminders' and store_key = 'A-1'")" ]; do sleep 1; done
db "select store_value from data_store where store_name = 'reminders' and store_key = 'A-1'" | python3 -c '
import json, sys
r = json.load(sys.stdin)
print(r["outcome"], "attempts=%s" % r["attempts"], r["patientReply"])'
```

```text
HTTP 202
answered attempts=1 STUB VOICE ANSWER: your appointment is confirmed for Tuesday at ten.
```

**5. Clean up:**

```bash
docker compose -p sutra-voice-assistant down -v
```

Full README, data and commands: [examples/voice-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/voice-assistant/README.md).
