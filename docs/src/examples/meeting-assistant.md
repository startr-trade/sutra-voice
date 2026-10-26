# meeting-assistant

A bot that dials into a conference by its SIP dial-in number, enters the meeting PIN, announces
that the call is recorded and transcribed, and then only listens. The transport records the meeting
from the end of the announcement on (sealed, referenced by hash in the audit trail) and transcribes
it live; the flow receives only sparse events. Afterwards it asks a model for minutes, decisions
and action items, applies the analysis policy (a decision table) and renders a report.

Because a dial-in is one mixed channel, speakers are not labelled by the transcript provider, and
the report says attribution is inferred. See [Mixed and per-participant media](../concepts/media-modes.md).

| Path | What |
|---|---|
| `shared/bpmn/meeting-call.bpmn` | the meeting: join, listen, report; the prompt for the minutes (FEEL) |
| `shared/bpmn/transcript-segment.bpmn` | stores each transcript segment as it completes |
| `shared/bpmn/scheduled-join.bpmn` | a standing meeting joined on a weekly timer |
| `shared/rules/analysis-policy.dmn` | who is analysed: external parties; internal staff only with the deployment's switch (`internalAnalysisAllowed`, false as shipped) and the request's opt-in |
| `variants/meeting-stub/`, `variants/meeting-live/` | the channels: the stub model, or real providers |
| `deploy/` | engine, PostgreSQL, MinIO, rustpbx, SIPp as the conference, the stub model |

## Walkthrough: join, transcribe, report, and a refused dial

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

**1. Package the example and start the stack:**

```bash
C=examples/meeting-assistant/deploy/compose.yaml
sutra package examples/meeting-assistant/variants/meeting-stub \
    --overlay examples/meeting-assistant/shared \
    --out examples/meeting-assistant/deploy/deployments | grep '^packaged'
docker compose -f $C --progress quiet up -d --build
until PORT=$(docker compose -f $C port engine 8080 2>/dev/null | cut -d: -f2) &&
      curl -sf "http://127.0.0.1:$PORT/sutra/deployments" | grep -q '"phase":"Active"'; do sleep 1; done
echo "the engine is on port $PORT"
```

```text
packaged examples/meeting-assistant/deploy/deployments/meeting-stub.sutra (deploymentId dep-8ef8bcdca12f51cc8c078bf1)
the engine is on port 33004
```

**2. Ask the bot to join.** The request asks for the internal participant to be analysed too
(`internalOptIn`), which this deployment does not allow:

```bash
curl -s -w "HTTP %{http_code}\n" -X POST "http://127.0.0.1:$PORT/channels/join" \
  -H 'X-Api-Key: dev-only-meeting-key' -H 'Content-Type: application/json' \
  -d '{"meetingId":"M-1","title":"Launch plan review","dialIn":"+15550142000","pin":"4321",
       "participants":[{"name":"Alex Chen","role":"internal"},{"name":"Sam Rivera","role":"external"}],
       "analysis":{"internalOptIn":true}}'
```

```text
HTTP 202
```

The bot dials the conference through rustpbx, sends `4321#` as telephone events, plays the
announcement, and listens. The stand-in conference (SIPp) plays about 11 s of a two-voice
conversation and hangs up.

**3. The transcript**, one row per final segment, stored while the call was up (no speaker
labels: a dial-in is one mixed channel):

```bash
db() { docker compose -f $C exec -T engine-db psql -U postgres -d sutra -tAc "$1"; }
until [ -n "$(db "select 1 from data_store where store_key = 'report:M-1'" 2>/dev/null)" ]; do sleep 2; done
db "select seq, speaker, start_ms, end_ms, text from transcripts where call_id = 'meeting-M-1' order by seq"
```

```text
1|unknown|9000|10960|Good morning, everyone. Let's go through the launch plan for the new booking page.
2|unknown|11900|13660|Thanks. The design is ready; the open question is the release date.
3|unknown|14600|16960|Then we agree to ship on the fifteenth. Action item: Priya books the accessibility review.
4|unknown|17900|19480|Agreed. I will send the summary to the client by Friday.
```

**4. The report**: the model's minutes, and the analysis policy applied to what it said about
each speaker:

```bash
db "select store_value from data_store where store_key = 'report:M-1'" | python3 -c '
import json, sys
r = json.load(sys.stdin)
print("summary:", r["summary"])
print("decisions:", r["decisions"])
print("analyzedRoles:", r["analyzedRoles"])
for s in r["speakers"]:
    print("speaker:", s["name"], s["role"], "analyzed=%s sentiment=%s emotion=%s" % (s["analyzed"], s["sentiment"], s["emotion"]))
print("keywords:", [k["phrase"] for k in r["keywords"]])
print("recording:", r["recording"]["uri"], r["recording"]["sha256"])'
```

```text
summary: The team reviewed the launch plan for the new booking page and set the release date.
decisions: ['Ship the new booking page on the fifteenth.']
analyzedRoles: ['external']
speaker: Alex Chen internal analyzed=False sentiment=None emotion=None
speaker: Sam Rivera external analyzed=True sentiment=positive emotion=engaged
keywords: ['action item']
recording: s3://meeting-recordings/calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed 79bfa05fb576c7efba5ccee37d935ba1e429387d0d029449e066dabed4484943
```

The decision table (`rules/analysis-policy.dmn`) said no for the internal role, because the
deployment's switch is off whatever the request asks, so the flow dropped the sentiment and
emotion the model returned for Alex.

**5. A dial the toll-fraud controls refuse.** The engine dials only what
`SUTRA_VOIP_ORIGINATE_ALLOW` lists (`+1555*` here). A meeting whose dial-in is elsewhere is
refused before anything is dialled, and the refusal is an event in that instance's own audit
trail:

```bash
curl -s -w "HTTP %{http_code}\n" -X POST "http://127.0.0.1:$PORT/channels/join" \
  -H 'X-Api-Key: dev-only-meeting-key' -H 'Content-Type: application/json' \
  -d '{"meetingId":"M-2","title":"Off the list","dialIn":"+447700900123","pin":"1111",
       "participants":[{"name":"Kim Park","role":"external"}]}'
until [ -n "$(db "select 1 from audit_event where event_type = 'voip.ORIGINATE_REFUSED'")" ]; do sleep 1; done
db "select event_type, diagnostic_code, payload_json from audit_event where event_type like 'voip.%'"
ID=$(db "select instance_id from audit_event where event_type = 'voip.ORIGINATE_REFUSED'")
db "select seq, event_type, coalesce(node_id, '') from audit_event where instance_id = '$ID' order by seq, id"
```

```text
HTTP 202
voip.ORIGINATE_REFUSED|SUTRA.VOIP.ORIGINATE_REFUSED|{}
1|INSTANCE_STARTED|
2|NODE_ENTERED|Start
3|NODE_LEFT|Start
4|NODE_ENTERED|Init
5|NODE_LEFT|Init
6|NODE_ENTERED|PolicyInputs
7|NODE_LEFT|PolicyInputs
8|NODE_ENTERED|Policy
9|NODE_LEFT|Policy
10|NODE_ENTERED|AnalysedRoles
11|NODE_LEFT|AnalysedRoles
12|NODE_ENTERED|PrepDial
13|NODE_LEFT|PrepDial
14|NODE_ENTERED|Dial
15|NODE_LEFT|Dial
16|NODE_ENTERED|Outcome
17|INSTANCE_SUSPENDED|
17|voip.ORIGINATE_REFUSED|
```

The event sits in the refused meeting's own journal, numbered with the instance's latest event (a
transport event takes the sequence number of the event it follows, so it never takes one the
engine writes next). Its payload is empty: the process audits at `capture="media"`, so no field (and never the
destination) is written.

**6. Clean up:**

```bash
docker compose -p sutra-meeting-assistant down -v
```

The [recording and consent guide](../guides/recording-and-consent.md#walkthrough-a-dial-in-recorded-after-consent)
walks through the recording itself on the same stack: the sealed stream, its manifest, and the
audited reference.

Full README, stub setup and live-provider notes: [examples/meeting-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/meeting-assistant/README.md).
