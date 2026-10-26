# Quick start

This runs the `ivr-dtmf` example: a DTMF phone menu with no model, on your machine, with no phone
line. [rustpbx](https://github.com/restsend/rustpbx) is the PBX, and SIPp plays the caller and the
phones of the extensions.

You need Docker, the `sutra` CLI ([installation](https://sutra.startr.trade/getting-started/installation.html))
and a clone of the repository with its submodule:

```bash
git clone --recurse-submodules https://github.com/startr-trade/sutra-voice
cd sutra-voice
```

## 1. Build the engine image

Until a 0.6 image is published, build it from the tree. The image carries the engine plus the
`voip` transport.

```bash
make image
export SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev
```

## 2. Package and start

```bash
sutra package examples/ivr-dtmf/package --out examples/ivr-dtmf/deploy/deployments
docker compose -f examples/ivr-dtmf/deploy/compose.yaml up -d --build
```

The four extension phones register with the PBX as they start; `docker compose ... logs ext-201`
shows `extension 201 registered`.

## 3. Place a test call

The caller takes the PBX address, the number it calls from, and the keys to press with pauses
between them. `2500ms` waits, `2` presses 2, and `4321#` presses 4, 3, 2, 1 and #. This call is
from an unknown number to a customers-only extension, so it is asked for a PIN:

```bash
docker compose -f examples/ivr-dtmf/deploy/compose.yaml run --rm caller \
    pbx:5060 +15550009999 2500ms 2 2500ms 4321#
```

## 4. Read the log

```bash
docker compose -f examples/ivr-dtmf/deploy/compose.yaml exec engine-db psql -U postgres -d sutra -c \
  "select caller, extension, outcome, method, reason, note, menu_attempts, pin_attempts, logged_at
     from transfer_log order by logged_at"
```

You should see one row with outcome `transferred`, method `pin` and the PIN's note. The digits
themselves are never logged. The example's page lists a call for every other path: an invalid
choice, a wrong or expired PIN, no choice at all.

## 5. Stop

```bash
docker compose -f examples/ivr-dtmf/deploy/compose.yaml down
```

Next: how it fits together in [Architecture](architecture/overview.md), or the
[ivr-dtmf example](examples/ivr-dtmf.md) in full.
