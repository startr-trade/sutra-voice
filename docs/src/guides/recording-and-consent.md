# Recording and consent

## When a call is recorded

| Call | Recorded when | Waits for consent when |
|---|---|---|
| Inbound | its `call-start` channel says `record: true` or `record: after-consent`, or the flow sends `voip://record-start` | `record: after-consent` |
| Originated | the `originate` request comes from a process that declares `<q:audit capture="media">` (the engine marks the request `sutra-audit-capture: media`), or the request says `record: true` or `record: "after-consent"` | `record: "after-consent"` |
| Meeting session | its `call-start` channel says `record: true` or `record: after-consent` (one recording per participant) | `record: after-consent`, which also holds back the session's segments |

A call to be recorded with no store configured is refused (`SUTRA.VOIP.RECORDING_UNAVAILABLE`).

## The consent rule

Consent is given on a call in one of two ways:

- **An announcement played to the end.** A `play` marked `consent: true` that is not interrupted
  (barge-in) records the moment it ended. The recording reference then says `consentAnnounced:
  true` and `consentAnnouncedAtMs`.
- **The flow vouching for it.** `voip://record-start` with `consent: true`, once the caller agreed
  some other way (pressed a key, say).

With `after-consent` the recording starts at that moment and not before: the audio before the
announcement, and the announcement itself, are not in it, and `consentAnnouncedAtMs` (where the
announcement started, relative to the recording's start) is negative. A call that ends before consent has no recording:
`recording: {status: "not-started", detail: "consent was never given, so nothing was recorded"}`.

`record: true` records from the moment the media connects and leaves consent to the deployment
(an announcement the PBX plays before the call reaches the engine, say).

## Record-start and record-stop

| Operation | Body | Answer |
|---|---|---|
| `voip://record-start` | `{callId, consent?: true}` | `{callId, status: "recording", uri, startMs, consent: "announced" \| "asserted"}`; refused with `SUTRA.VOIP.CONSENT_REQUIRED` (and a `voip.CONSENT_REQUIRED` audit event) until consent was given |
| `voip://record-stop` | `{callId}` | `{callId, status: "stopped", recording: <reference>}`, or `status: "not-recording"` |

A call can be recorded in several stretches: the second recording of call `c1` is `c1-2.wav`, and
the `call-hangup` carries every one in `recordings` (the last in `recording`). On a meeting session
both operations act on every present participant.

## The reference

Neither the flow nor the audit trail ever holds audio. The `call-hangup` (and `record-stop`)
carries a **media reference**:

| Field | Meaning |
|---|---|
| `kind` | `"media"`: what `<q:audit capture="media">` collects from the instance's variables |
| `uri` | `s3://<bucket>/<prefix>/<tenant>/<module>/<version>/<callId>.wav.sealed` |
| `sha256`, `bytes` | the SHA-256 and size of the stored object, so its integrity can be checked without the key |
| `durationMs`, `format`, `rate`, `channels` | the audio: `audio/wav`, mono PCM16 at the call's rate |
| `encryption` | `{scheme: "sutra-sealed-stream-1", cipher: "aes-256-gcm", keyId}`, or null when unsealed |
| `startMs` | where in the call (ms since the media connected) the recording begins |
| `consentAnnounced`, `consentAnnouncedAtMs` | whether the announcement played to the end, and where it started, relative to the recording's start (negative: before the recording began) |
| `complete` | `true`: the stream verified to its final frame. A recording recovered after a crash says `false` |
| `recovered` | present and `true` on a recording the sweep recovered |
| `truncated` | audio beyond `SUTRA_VOIP_RECORDING_MAX_SECONDS` was not recorded |
| `participantId`, `participantIndex` | a meeting participant's recording |

A store that cannot be written answers `recording: {status: "failed", detail}`.

## How a recording is written

The recording **streams** to `SUTRA_VOIP_RECORDING_STORE` while the call is up. Memory does not
grow with the call:

1. The audio (the far end, silence-filled where the gateway sent nothing so the recording keeps
   the call's clock, with the transport's own prompts mixed in where they played) is cut into
   one-second chunks.
2. Each chunk is sealed and appended to a spool file (`SUTRA_VOIP_RECORDING_SPOOL_DIR`).
3. Every time the spool holds 5 MiB (the smallest part an S3 store accepts) it is uploaded as the
   next part of a multipart upload, and the spool is emptied.
4. At the hang-up the rest is uploaded, the upload completed, and the reference delivered.

The engine's object store client (`sutra-objectstore`) does the uploads, so any S3-compatible
store works.

### The sealed-stream format

```text
"SUTRA-SEALED-STREAM-1\n"     magic, 22 bytes
u32 big-endian                length of the header
header                        JSON: {scheme, keyId, wrappedKey (base64), contentType, cipher}
frame*                        u32 big-endian length || nonce (12) || AES-256-GCM(chunk) || tag (16)
```

One fresh data key per recording, wrapped under `SUTRA_VOIP_RECORDING_KEK` (the engine's envelope
encryption). Each frame's associated data binds it to the key id, the object key, its index and
whether it is the last one, so frames cannot be reordered, dropped, moved to another object or
passed off as the end. Frame 0 is the WAV header and the last frame is marked final, so a
truncated object verifies up to its last good frame and reads as incomplete.
`sutra_transport_voip::recording::open_stream` opens one.

The WAV header is written before the length is known, so it carries the streaming sizes
`0xFFFFFFFF` in its RIFF and data fields. `open_stream` answers the file with its exact sizes; a
plain (`encryption: none`) recording keeps the streaming sizes, which most players accept.

### The manifest and the markers

Each recording has a manifest, `<key>.json` next to it in the store (and in the spool directory):
the call it belongs to (`callId`, `tenant`, `moduleKey`, a meeting's `participantId`), the audit
routing of the instance that owns the call, who is writing it (`owner`, `boot`), its upload id,
the next part number, and its `status`: `recording`, `complete` (with `sha256`, `bytes`),
`failed`, `recovered` or `empty`. While the upload is in progress a copy of the manifest is also
kept under `sutra-voip-uploads/` (a marker), because some S3-compatible stores list multipart
uploads only by exact key; the marker is deleted when the recording completes.

### Crash recovery

A sweep runs at start-up and every `SUTRA_VOIP_RECORDING_SWEEP_SECONDS` (600). It finds the
recordings no live call is writing:

- this host's spools left by an earlier run of the process (`SUTRA_VOIP_INSTANCE_ID`, default the
  host name, says who "this host" is), and
- markers in the store of uploads nobody has touched for longer than the longest recording plus
  ten minutes (another host's, or one whose spool is gone).

For each, it uploads what the spool still holds, completes the upload, verifies the stored object
frame by frame, and amends the manifest (`status: "recovered"`, `complete: false` unless the final
frame made it, the captured `durationMs`, `sha256`). An upload with no audio is aborted
(`status: "empty"`). Unless the call's hang-up already went out, the reference is then delivered
on the call's `call-hangup` channel with `reason: "recording-recovered"` and `recovered: true`.
Both steps are audit events in the owning instance's journal (`voip.RECORDING_RECOVERED`,
`voip.RECORDING_RECOVERED_DELIVERED`).

## Walkthrough: a dial-in recorded after consent

`examples/meeting-assistant` dials a conference, enters the PIN, plays the consent announcement
and listens. Its originate asks `record: "after-consent"`, from a process that audits at
`capture="media"`. This walkthrough shows the recording starting after the announcement, the
sealed stream and its manifest in the bucket, and the audit trail carrying only the reference.

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

**1. Package the example and start the stack** (engine, PostgreSQL, MinIO, rustpbx, SIPp as the
conference, the stub model):

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
the engine is on port 32995
```

(`sutra package` also prints three warnings about the schema-less JSON channels; the `grep`
leaves them out. The engine's port is dynamic.)

**2. Ask the bot to join the meeting:**

```bash
curl -s -w "HTTP %{http_code}\n" -X POST "http://127.0.0.1:$PORT/channels/join" \
  -H 'X-Api-Key: dev-only-meeting-key' -H 'Content-Type: application/json' \
  -d '{"meetingId":"M-1","title":"Launch plan review","dialIn":"+15550142000","pin":"4321",
       "participants":[{"name":"Alex Chen","role":"internal"},{"name":"Sam Rivera","role":"external"}]}'
```

```text
HTTP 202
```

The conference answers, the bot enters the PIN and plays the 1.6 s announcement, and the
recording starts when the announcement ends. The stand-in conference then plays about 11 s of
conversation and hangs up.

**3. Read the recording's reference** from the stored report, once the flow has written it
(about 25 s after the join):

```bash
db() { docker compose -f $C exec -T engine-db psql -U postgres -d sutra -tAc "$1"; }
until [ -n "$(db "select 1 from data_store where store_key = 'report:M-1'" 2>/dev/null)" ]; do sleep 2; done
db "select store_value from data_store where store_key = 'report:M-1'" | python3 -c '
import json, sys
r = json.load(sys.stdin)["recording"]
for k in ["kind", "uri", "format", "durationMs", "startMs", "consentAnnounced", "consentAnnouncedAtMs", "complete", "encryption", "sha256"]:
    print(f"{k}: {json.dumps(r[k])}")'
```

```text
kind: "media"
uri: "s3://meeting-recordings/calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed"
format: "audio/wav"
durationMs: 22995
startMs: 2617
consentAnnounced: true
consentAnnouncedAtMs: -1581
complete: true
encryption: {"cipher": "aes-256-gcm", "keyId": "recordings", "scheme": "sutra-sealed-stream-1"}
sha256: "829bf816c534c718a1e310b561220d4747d1257242baec7cbb414beebdfd673f"
```

`startMs` is where in the call the recording began, 2.6 s after the media connected: after the PIN
and the announcement. `consentAnnouncedAtMs` is where the announcement started, relative to the
recording's own start: it began 1.6 s before the recording, and the recording started when it
ended. (The `data_store` table is created on the first write, so the loop's first probes fail
quietly.)

**4. The objects in the bucket.** The MinIO image carries the `mc` client; point it at the
server, then list the recording, its manifest, and the (now empty) markers prefix:

```bash
docker compose -f $C exec -T recordings mc alias set local http://localhost:9000 dev-only-access dev-only-secret-key >/dev/null
docker compose -f $C exec -T recordings mc ls -r local/meeting-recordings
```

```text
[2026-10-26 07:20:33 UTC] 360KiB STANDARD calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed
[2026-10-26 07:20:33 UTC]   983B STANDARD calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed.json
```

The recording and its manifest. No marker is left under `sutra-voip-uploads/`: the upload
completed.

**5. The manifest and the sealed stream:**

```bash
KEY=calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed
docker compose -f $C exec -T recordings mc cat local/meeting-recordings/$KEY.json | python3 -c '
import json, sys
m = json.load(sys.stdin)
for k in ["scheme", "status", "complete", "callId", "moduleKey", "sealed", "keyId", "nextPart", "hangupDelivered", "audit", "sha256", "bytes"]:
    print(f"{k}: {json.dumps(m[k])}")'
docker compose -f $C exec -T recordings mc cat local/meeting-recordings/$KEY | head -c 22; echo
docker compose -f $C exec -T recordings mc cat local/meeting-recordings/$KEY | sha256sum
```

```text
scheme: "sutra-sealed-stream-1"
status: "complete"
complete: true
callId: "meeting-M-1"
moduleKey: "default/meeting-assistant/1.0.0"
sealed: true
keyId: "recordings"
nextPart: 2
hangupDelivered: true
audit: {"sutra-audit-journal": "capture=media;sink=sql", "sutra-audit-deployment": "dep-8ef8bcdca12f51cc8c078bf1", "sutra-audit-instance": "fbbf6c70-3d45-4395-aaaa-38d5add0e952"}
sha256: "829bf816c534c718a1e310b561220d4747d1257242baec7cbb414beebdfd673f"
bytes: 368990
SUTRA-SEALED-STREAM-1
829bf816c534c718a1e310b561220d4747d1257242baec7cbb414beebdfd673f  -
```

A 360 KiB recording fits in one part (`nextPart: 2`). `audit` is the routing of the instance that
owns the call, kept so a recovery would write its audit events to that instance's journal. The
object starts with the sealed-stream magic, and its SHA-256 is the reference's: the integrity of
the recording can be checked without the key.

**6. The audit trail carries the reference, never the audio:**

```bash
db "select event_type, node_id, payload_json from audit_event where payload_json like '%\"media\"%' order by seq desc limit 1"
```

```text
NODE_ENTERED|End|{"media":[{"bytes":368990,"callId":"meeting-M-1","channels":1,"complete":true,"consentAnnounced":true,"consentAnnouncedAtMs":-1581,"durationMs":22995,"encryption":{"cipher":"aes-256-gcm","keyId":"recordings","scheme":"sutra-sealed-stream-1"},"format":"audio/wav","kind":"media","rate":8000,"sha256":"829bf816c534c718a1e310b561220d4747d1257242baec7cbb414beebdfd673f","startMs":2617,"truncated":false,"uri":"s3://meeting-recordings/calls/default/meeting-assistant/1.0.0/meeting-M-1.wav.sealed"}]}
```

**7. Clean up:**

```bash
docker compose -p sutra-meeting-assistant down -v
```

## Consent in practice

Recording and analysis rules differ by jurisdiction: some require every party's consent, some an
announcement, some a way to opt out. The software cannot decide which applies; the flow carries the
announcement, and the platform makes it provable.

- **Say it before anything is recorded.** Play the announcement as the first step, with
  `consent: true` on the `play`, and record `after-consent`. The recording reference then states
  `consentAnnounced` and `consentAnnouncedAtMs`.
- **Meeting bots are visible.** The WebRTC worker joins under a display name that says it records
  (default `Meeting assistant (recording)`), and the example flow announces recording and analysis
  in the room before anything is recorded or cut into segments (`record: after-consent`).
- **Personal data.** Mark stores holding interaction records as personal, audit metadata and not
  payloads, treat the caller's turns as sensitive variables so they are redacted wherever they would
  be logged, and mark the caller number as a subject key so erasure requests find it. Recordings and
  segments are referenced from the audit trail, so the engine's subject erasure and retention
  delete them (see the engine book's "Recordings and media references").
- **A menu that does not record says so by not recording.** The `ivr-dtmf` example neither records
  nor analyses; if you add either, update the welcome prompt first.

The full key list is in [Configuration](../operations/configuration.md). This page is engineering
guidance, not legal advice.
