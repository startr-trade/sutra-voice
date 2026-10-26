# Quick start

This runs the `ivr-dtmf` example: a DTMF phone menu with no model, on your machine, with no phone
line. [rustpbx](https://github.com/restsend/rustpbx) is the PBX, and SIPp plays the caller and the
phones of the extensions. The output below was copied from a real run; ids, ports and times differ
on every run.

## Prerequisites

Docker with Compose v2, and a clone of the repository with its submodule:

```bash
git clone --recurse-submodules https://github.com/startr-trade/sutra-voice
cd sutra-voice
```

Until 0.6.0 is published, build the engine image (the engine plus the `voip` transport) and the
`sutra` CLI from the clone. The CLI comes from the engine submodule; its first build takes several
minutes.

```bash
make image
cargo build --manifest-path engine/rust/Cargo.toml --profile release-it -p sutra-cli --target-dir rust/target
export PATH="$PWD/rust/target/release-it:$PATH"
export SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev
```

## 1. Package and start

```bash
C=examples/ivr-dtmf/deploy/compose.yaml
sutra package examples/ivr-dtmf/package --out examples/ivr-dtmf/deploy/deployments | grep '^packaged'
docker compose -f $C --progress quiet up -d --build
until PORT=$(docker compose -f $C port engine 8080 2>/dev/null | cut -d: -f2) &&
      curl -sf "http://127.0.0.1:$PORT/sutra/deployments" | grep -q '"phase":"Active"'; do sleep 1; done
until docker compose -f $C logs ext-203 | grep -q registered; do sleep 1; done
docker compose -f $C logs ext-201 | grep -o 'extension 201 registered'
```

```text
packaged examples/ivr-dtmf/deploy/deployments/package.sutra (deploymentId dep-8051dbc2291932f5c9d64341)
extension 201 registered
```

The four extension phones register with the PBX as they start.

## 2. Place a test call

The caller takes the PBX address, the number it calls from, and the keys to press with pauses
between them. `2500ms` waits, `2` presses 2, and `4321#` presses 4, 3, 2, 1 and #. This call is
from an unknown number to a customers-only extension (support, 202), so it is asked for a PIN:

```bash
docker compose -f $C --progress quiet run --rm caller \
    pbx:5060 +15550009999 2500ms 2 2500ms 4321# >/dev/null 2>&1; echo "the caller exited with $?"
```

```text
the caller exited with 0
```

## 3. Read the log

```bash
docker compose -f $C exec -T engine-db psql -U postgres -d sutra -c \
  "select caller, extension, outcome, method, reason, note, menu_attempts, pin_attempts
     from transfer_log order by logged_at"
```

```text
    caller    | extension |   outcome   | method | reason |               note                | menu_attempts | pin_attempts 
--------------+-----------+-------------+--------+--------+-----------------------------------+---------------+--------------
 +15550009999 | 202       | transferred | pin    |        | shared with the partner help desk |             1 |            1
(1 row)
```

One row with outcome `transferred`, method `pin` and the PIN's note. The digits themselves are
never logged. The extension's phone noted the call it answered:

```bash
docker compose -f $C exec -T ext-202 cat /capture/ext-202.log
```

```text
answered
```

## 4. Another path: a wrong PIN, three times

The [ivr-dtmf example](examples/ivr-dtmf.md) has a call for every path. This one enters three
wrong PINs for accounts (203), and the menu gives up and hangs up:

```bash
docker compose -f $C --progress quiet run --rm caller \
    pbx:5060 +15550009999 2500ms 3 2500ms 0000# 2500ms 1111# 2500ms 2222# >/dev/null 2>&1; echo "the caller exited with $?"
docker compose -f $C exec -T engine-db psql -U postgres -d sutra -c \
  "select extension, outcome, reason, pin_attempts from transfer_log where extension = '203' order by logged_at"
```

```text
the caller exited with 0
 extension |   outcome   |         reason         | pin_attempts 
-----------+-------------+------------------------+--------------
 203       | pin-refused | wrong                  |            1
 203       | pin-refused | wrong                  |            2
 203       | pin-refused | wrong                  |            3
 203       | gave-up     | pin-attempts-exhausted |            3
(4 rows)
```

## 5. Stop

```bash
docker compose -p sutra-ivr-dtmf down -v
```

Next: how it fits together in [Architecture](architecture/overview.md), or the
[ivr-dtmf example](examples/ivr-dtmf.md) in full.
