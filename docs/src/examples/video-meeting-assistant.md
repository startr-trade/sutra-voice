# video-meeting-assistant

A bot in a LiveKit room, through the [WebRTC worker](../guides/webrtc-worker.md). It records and
transcribes each participant's own track. The transport cuts each external participant's media
into 5 s segments and hands the flow their references; the flow decides who is analysed (a
decision table), asks a video-understanding model about each segment with the media as
references, keeps only what it asked for, and builds a per-participant digest and a report after
the meeting.

Analysis covers external participants by default; staff are opt-in only, and in this example
never even cut into segments. Read [Emotion analytics and compliance](../guides/emotion-analytics.md)
before changing that. The stack runs with a stub model and two scripted participants, no keys.

| Path | What |
|---|---|
| `shared/bpmn/video-meeting.bpmn` | the session: the consent announcement, the roster (with each participant's policy), then the rows read back, the digest and the report |
| `shared/bpmn/segment-analysis.bpmn` | one segment of one participant: the policy, the prompt (FEEL), the model call by reference, the fusion (FEEL), the row |
| `shared/bpmn/transcript-segment.bpmn` | stores each transcript segment |
| `shared/rules/analysis-policy.dmn` | who is analysed |
| `variants/video-stub/channels.yaml` | the channels: `call-segment` (`roles: [external]`), `video-analysis` (`media-store: recordings`) |
| `deploy/compose.yaml` | LiveKit, the worker, the engine (with the object store `recordings` over the recording bucket), MinIO, PostgreSQL, the stub model, and the scripted participants (`--profile demo`) |

## Walkthrough: a meeting, analysed in the flow

**Prerequisites.** Docker with Compose v2 and `python3`, and a checkout of this repository with
its submodule. Until 0.6.0 is published, build the engine image, the worker image (its first build
downloads libwebrtc and takes several minutes) and the `sutra` CLI from the checkout:

```bash
git submodule update --init
make image worker-image
cargo build --manifest-path engine/rust/Cargo.toml --profile release-it -p sutra-cli --target-dir rust/target
export PATH="$PWD/rust/target/release-it:$PATH"
export SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev
```

**1. Package the example and start the stack.** The worker joins the room `standup` and waits:

```bash
C=examples/video-meeting-assistant/deploy/compose.yaml
sutra package examples/video-meeting-assistant/variants/video-stub \
    --overlay examples/video-meeting-assistant/shared \
    --out examples/video-meeting-assistant/deploy/deployments | grep '^packaged'
docker compose -f $C --progress quiet up -d
until docker compose -f $C logs worker | grep -q 'joined the room'; do sleep 1; done
echo "the bot is in the room"
```

```text
packaged examples/video-meeting-assistant/deploy/deployments/video-stub.sutra (deploymentId dep-5b4edf77ce8251f956940453)
the bot is in the room
```

**2. Two participants join**, scripted (a synthetic microphone on a talk script, a synthetic
camera): Alex, admitted as `staff`, and Sam, admitted as `external`. Each stays 16 s:

```bash
docker compose -f $C --progress quiet --profile demo up -d alex sam
db() { docker compose -f $C exec -T engine-db psql -U postgres -d sutra -tAc "$1"; }
until [ -n "$(db "select 1 from data_store where store_key like 'session:%'" 2>/dev/null)" ]; do sleep 2; done
db "select store_name, count(*) from data_store group by 1 order by 1"
```

```text
analysis|4
meetings|1
transcripts|4
```

The session is over about 30 s after they join. The bot played the announcement first; the session
is `record: after-consent`, so nothing was recorded, and no segment cut, before it ended.

**3. Who was analysed, and why.** The roster's decisions, from the decision table:

```bash
db "select store_value from data_store where store_key like 'session:%'" | python3 -c '
import json, sys
r = json.load(sys.stdin)
for p in r["participants"]:
    print(p["label"], p["role"], "segmented=%s analysed=%s policy=%s segments=%s emotion=%s engagement=%s disagreements=%s"
          % (p["segmented"], p["analysed"], p["analysisPolicy"], p["segments"], p["emotion"], p["engagement"], p["disagreements"]))
a = r["analysis"]
print("analysis:", a["channel"], a["provider"], a["model"], "results=%s failed=%s" % (a["results"], a["failed"]))'
```

```text
Alex staff segmented=False analysed=False policy=staff-not-opted-in segments=0 emotion=None engagement=None disagreements=0
Sam external segmented=True analysed=True policy=analysed segments=4 emotion=interested engagement=high disagreements=0
analysis: video-analysis gemini gemini-video-stub results=4 failed=0
```

**4. One analysed segment.** The row keeps the media references (never the media), what the model
received (the stub reports it), the fused result, and what the flow dropped:

```bash
db "select store_value from data_store where store_name = 'analysis' order by store_key limit 1" | python3 -c '
import json, sys
a = json.load(sys.stdin)
print("segment", a["segment"], a["startMs"], "-", a["endMs"], "ms:", a["status"], a["analysisPolicy"])
print("audio:", a["media"]["audio"]["uri"].split("/1.0.0/")[1], a["media"]["audio"]["sha256"][:16])
print("frames:", [f["uri"].split("/segments/")[1] for f in a["media"]["frames"]])
print("the model saw:", a["seen"])
print("emotion:", json.dumps(a["result"]["emotion"]))
print("disagreement:", json.dumps(a["result"]["disagreement"]))
print("removedKeys:", a["removedKeys"], "unsupportedClaims:", a["unsupportedClaims"])'
```

```text
segment 1 0 - 5000 ms: complete analysed
audio: standup-1793000726015/segments/p2/1.wav.sealed d230f38962afbd98
frames: ['p2/1-f1.jpg.sealed', 'p2/1-f2.jpg.sealed', 'p2/1-f3.jpg.sealed', 'p2/1-f4.jpg.sealed', 'p2/1-f5.jpg.sealed', 'p2/1-f6.jpg.sealed', 'p2/1-f7.jpg.sealed', 'p2/1-f8.jpg.sealed']
the model saw: {'audio': True, 'frames': 8}
emotion: {"confidence": 0.6, "label": "interested", "modalities": {"body": true, "face": true, "voice": true, "words": true}}
disagreement: {"detail": null, "flagged": false}
removedKeys: ['audioSeen', 'framesSeen'] unsupportedClaims: []
```

The `video-analysis` channel read each referenced object from its `media-store`, checked its
SHA-256, opened it and sent it: the stub counts the frames and the audio it received. The stub's
own bookkeeping keys (`framesSeen`, `audioSeen`) are not fields the flow asked for, so they are
dropped and listed, as an inferred age or identity would be.

**5. The segments in the bucket:** Sam's only (participant `p2` in this run: indexes follow the
order people joined); Alex's media was never written as a segment.

```bash
docker compose -f $C exec -T recordings mc alias set local http://localhost:9000 dev-only-access dev-only-secret-key >/dev/null
docker compose -f $C exec -T recordings mc ls -r local/meeting-recordings | awk '{print $NF}' | sed 's|.*/1.0.0/[^/]*/||' | sort
```

```text
p1.wav.sealed
p1.wav.sealed.json
p2.wav.sealed
p2.wav.sealed.json
segments/p2/1-f1.jpg.sealed
segments/p2/1-f2.jpg.sealed
segments/p2/1-f3.jpg.sealed
segments/p2/1-f4.jpg.sealed
segments/p2/1-f5.jpg.sealed
segments/p2/1-f6.jpg.sealed
segments/p2/1-f7.jpg.sealed
segments/p2/1-f8.jpg.sealed
segments/p2/1.wav.sealed
segments/p2/2-f1.jpg.sealed
segments/p2/2-f2.jpg.sealed
segments/p2/2-f3.jpg.sealed
segments/p2/2-f4.jpg.sealed
segments/p2/2-f5.jpg.sealed
segments/p2/2-f6.jpg.sealed
segments/p2/2.wav.sealed
segments/p2/3-f1.jpg.sealed
segments/p2/3-f2.jpg.sealed
segments/p2/3-f3.jpg.sealed
segments/p2/3-f4.jpg.sealed
segments/p2/3-f5.jpg.sealed
segments/p2/3-f6.jpg.sealed
segments/p2/3.wav.sealed
segments/p2/4-f1.jpg.sealed
segments/p2/4-f2.jpg.sealed
segments/p2/4.wav.sealed
```

Each participant's recording (`p1.wav.sealed`, `p2.wav.sealed`) with its manifest, and Sam's
segments: the audio of each and its frames, sealed.

**6. The report:**

```bash
db "select store_value from data_store where store_key like 'session:%'" | python3 -c '
import json, sys; print(json.load(sys.stdin)["report"])' | sed -n '/## Participants/,/^## How/p'
```

```text
## Participants
- P1 Alex (staff): about 5940 ms of speech, 2 transcript segments; not analysed (staff-not-opted-in).
  Recording: s3://meeting-recordings/sessions/default/video-meeting-assistant/1.0.0/standup-1793000726015/p1.wav.sealed (SHA-256 f4a7f8732dca24685bd6d90aa4f83fa0c4c3dd13a2ca4603794dcc9125835e94)
- P2 Sam (external): about 5940 ms of speech, 2 transcript segments; analysed over 4 segments — mostly interested, engagement high, 0 segments where voice, face and words disagreed.
  Recording: s3://meeting-recordings/sessions/default/video-meeting-assistant/1.0.0/standup-1793000726015/p2.wav.sealed (SHA-256 787e03527cfba86a29afbffc4727a89d3a947095c71350a4a216b1447881228c)

## How this was produced
```

**7. Clean up:**

```bash
docker compose -p sutra-video-meeting-assistant down -v
```

Full README, compose stack and cost notes: [examples/video-meeting-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/video-meeting-assistant/README.md).
