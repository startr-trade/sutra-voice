# ivr-dtmf — a phone menu with keys, extensions and PINs

A classic phone menu, end to end, with no model involved:

- **A number with a menu.** A call to `+15551239000` hears a recorded welcome and menu. The
  caller presses a key; a decision table (DMN) turns it into an extension and an access rule.
  A key the menu does not offer — or no key within 6 s — plays "not a valid choice" and the
  menu again, up to 3 times, then "sorry, goodbye" and the menu hangs up.
- **Open and customers-only extensions.** An open extension is transferred to at once. A
  customers-only one first looks the caller's number up in the `customers` store: a customer
  is transferred; anyone else is asked for a PIN (keys, ending with `#`), checked against
  `extension_pins`. A valid PIN transfers the call and logs the PIN's note — who it was shared
  with; a wrong, inactive or expired one is refused and asked for again, 3 tries in all.
- **A log of every outcome.** `transfer_log` records each transfer (caller, extension, how it
  was allowed, the PIN's note, when), each refused PIN (and why), and each call that gave up or
  hung up — never the digits the caller pressed.

It runs locally with no phone line: [rustpbx](https://github.com/restsend/rustpbx) is the PBX,
SIPp plays the carrier's caller and the phones of the extensions.

The whole call is **one instance** of `ivr-call.bpmn`, and both retry rules are real loops: the
menu loops back to the same wait for a key, and so does PIN entry — once per key until `#`, and
once per attempt.

## What is in here

```
package/                            the ivr-dtmf module (one deployment package)
  bpmn/ivr-call.bpmn                  the call: menu loop, access rule, PIN loop, transfer, log
  rules/menu.dmn                      key -> extension, department, access rule — edit the menu here
  schemas/ivr/ivr.xsd                 the row types of the three stores (Customer, ExtensionPin, TransferLogEntry)
  datastores.yaml                     customers, extension_pins, transfer_log — each a typed table
  migrations/<store>/V001__*.sql      each store's DDL and its example data
  channels.yaml                       the call's events, and play / transfer / hang-up
prompts/                            the recorded prompts (WAV) and the script that made them
deploy/compose.yaml                 engine + PostgreSQL + rustpbx + four extension phones + a caller
deploy/rustpbx/config.toml          the PBX: trunk, the menu number's route, extensions, call control
deploy/sipp/                        the stand-in caller and phones (scenarios, RFC 2833 key presses)
```

## Run it

You need the `sutra` CLI and Docker. Until the 0.6.0 image is published, build it from this tree
(`make image`) and set `SUTRA_ENGINE_IMAGE=sutra-voice-engine:dev`.

```bash
sutra package examples/ivr-dtmf/package --out examples/ivr-dtmf/deploy/deployments
docker compose -f examples/ivr-dtmf/deploy/compose.yaml up -d --build
```

The four extension phones register with the PBX as they start (`docker compose … logs ext-201`
shows `extension 201 registered`).

**Place a call.** The caller takes the PBX address, the number it calls from, and the keys to
press with the pauses between them — `2500ms` waits, `2` presses 2, `4321#` enters 4, 3, 2, 1
and #. It stays on the line for 8 s after the last key (`HOLD_SECS`) unless the call ends first.

```bash
docker compose -f examples/ivr-dtmf/deploy/compose.yaml run --rm caller \
    pbx:5060 +15550009999 2500ms 2 2500ms 4321#
```

**Read the log:**

```bash
docker compose -f examples/ivr-dtmf/deploy/compose.yaml exec engine-db psql -U postgres -d sutra -c \
  "select caller, extension, outcome, method, reason, note, menu_attempts, pin_attempts, logged_at
     from transfer_log order by logged_at"
```

Each extension's phone notes the calls it answered: `docker compose … exec ext-202 cat
/capture/ext-202.log`.

Stop it with `docker compose -f examples/ivr-dtmf/deploy/compose.yaml down`.

## The example data — and how to try every path

The stores' migrations seed this data (fictional numbers), so every path can be tried.

**The menu** (`rules/menu.dmn`):

| Key | Extension | Department | Access |
|---|---|---|---|
| 1 | 201 | sales | open |
| 2 | 202 | support | customers only |
| 3 | 203 | accounts | customers only |
| 0 | 200 | front desk | open |
| anything else | — | — | not a choice: the menu again |

**Customers** (`customers`): `+15557654321` (Avery Example), `+15550002222` (Blake Example).
Any other number is not a customer.

**PINs** (`extension_pins`):

| Extension | PIN | Note (logged with the transfer) | Active | Expires |
|---|---|---|---|---|
| 202 | 4321 | shared with the partner help desk | yes | never |
| 202 | 9999 | old contractor PIN, switched off | **no** | never |
| 203 | 2468 | given to the auditors for this year | yes | 2099-12-31 |
| 203 | 1357 | trial partner access, ended | yes | **2024-01-31 (expired)** |

| To try | Keys (after `pbx:5060 <from>`) | From | Logged |
|---|---|---|---|
| an open extension | `2500ms 1` | any number | `transferred`, method `open` |
| a customer, no PIN | `2500ms 2` | `+15557654321` | `transferred`, method `customer` |
| an unknown caller with a valid PIN | `2500ms 2 2500ms 4321#` | `+15550009999` | `transferred`, method `pin`, the note |
| an inactive PIN | `2500ms 2 2500ms 9999#` | `+15550009999` | `pin-refused`, reason `inactive` |
| an expired PIN | `2500ms 3 2500ms 1357#` | `+15550009999` | `pin-refused`, reason `expired` |
| a wrong PIN three times | `2500ms 3 2500ms 0000# 2500ms 1111# 2500ms 2222#` | `+15550009999` | three `pin-refused` (`wrong`), then `gave-up`; the menu hangs up |
| an invalid choice, then a valid one | `2500ms 9 2500ms 1` | any number | `transferred` with `menu_attempts` 2 |
| no choice at all | `25000ms` | any number | `gave-up`, reason `no-valid-choice` |

A caller who hangs up while the menu or the PIN entry waits is logged as `hung-up`. Keys pressed
while a prompt is playing are kept and handled, in order, as soon as it ends — a caller who knows
the menu can type ahead.

## The prompts

`prompts/` holds one WAV file per prompt. **They are not speech**: no text-to-speech engine is
assumed to be available offline, so `prompts/make-prompts.py` writes each as a short, distinct
pattern of tones (deterministically — re-running it writes the same bytes). What each stands for:

| File | A real recording would say |
|---|---|
| `welcome.wav` | "Welcome. For sales press 1, for support press 2, for accounts press 3, for the front desk press 0." |
| `menu.wav` | "For sales press 1, for support 2, for accounts 3, for the front desk 0." |
| `invalid.wav` | "Sorry, that is not a valid choice." |
| `enter-pin.wav` | "Please enter your PIN, then press hash." |
| `wrong-pin.wav` | "That PIN is not valid." |
| `transferring.wav` | "Transferring you now." |
| `goodbye.wav` | "Sorry, we could not connect your call. Goodbye." |

**To use real recordings,** replace the files with recordings of the same names: WAV, any sample
rate, PCM16, µ-law or A-law — the engine converts them to the call's rate. Nothing needs
repackaging: the engine reads them from its `SUTRA_VOIP_PROMPTS_DIR` (compose mounts `prompts/`
at `/prompts/ivr-dtmf`) each time it plays one, and the flow names them (`ivr-dtmf/menu`). To add a
prompt, add a file and name it in a `play` request: `{callId, prompt: "ivr-dtmf/<file name>"}`.

## Change it

- **The menu:** `rules/menu.dmn` — keys, extensions, departments and access rules.
- **How many tries, how long to wait:** `maxMenuAttempts` / `maxPinAttempts` in the `Init` task
  of `ivr-call.bpmn` (3 each), and the timers on the two waits (6 s for a choice, 8 s between PIN
  keys).
- **Customers and PINs:** the stores' tables. The seed rows are inserted `ON CONFLICT DO NOTHING`,
  so rows you add or change survive a redeploy (the migrations re-run on every activation).
- **The PBX domain:** the transfer target is `sip:<extension>@pbx` (`PrepTransfer` in the BPMN);
  use your PBX's domain.

## PINs are in clear text here — deliberately

The `extension_pins` table holds PINs **in clear text**. That is a deliberate choice for the
example, so the data stays readable and every path can be tried by looking at the table; it is
an accepted risk for a local example only. Anyone who can read that table (or a backup of it)
can reach the protected extensions.

**For production, do not keep PINs in the engine's data stores.** Check them through a small
PIN-verification service deployed alongside the engine, which keeps **salted hashes** of the
PINs in its own store and answers whether a PIN is valid:

```yaml
# channels.yaml: a synchronous call — the flow waits for the answer
  - name: verify-pin
    direction: outbound
    transport: http
    bind: "http://pin-verifier:8080/verify"
    codec: json
    ack-mode: on-complete
```

The flow then replaces the `CheckPin` lookup with a service task on `channel:verify-pin`, sending
`{extension, pin}` and reading back `{valid, note}` (plus a reason when refused); the rest of the
flow is unchanged. The PIN then exists only in the call's own variables, for the moment it is
checked — and those are tagged `sensitive` (`pinDigits`, `pinKey` in `ivr-call.bpmn`), so logs,
audit sinks and diagnostics redact them, and the process audits metadata only. The flow clears
the digits as soon as they have been checked, and no `transfer_log` row ever holds them.

## The extensions' phones

The four extensions register with the PBX using SIP digest and the **dev-only** passwords in
`deploy/rustpbx/config.toml` (`[[proxy.user_backends]]`) and `compose.yaml`. Replace them, and
restrict `acl_rules`, before letting real phones register. To use real phones instead of SIPp,
point them at the PBX (publish its SIP port and RTP range, set `external_ip`) with those
credentials, and remove the `ext-*` services.

## A real trunk

The same steps as `examples/voice-assistant` ("Switch to a real trunk"): point
`[proxy.trunks.carrier]` at your carrier, set `did_numbers` and the route's `"to.user"` match to
your number, and change the `call-start` channel's `bind: "number:…"` in `package/channels.yaml`.

## Consent and announcements

This menu does not record calls or analyse them. If you add either — a `q:audit` media
recording, or a model listening to the caller — say so in the welcome prompt before anything is
recorded, and check what your jurisdiction requires (some need every party's consent, some an
announcement, some a way to opt out).

## Tests

The tier-2 suite (`rust/crates/sutra-voice-conformance/tests/all/tc_ivr_dtmf.rs`) runs this
example on a Docker network — the engine image, rustpbx pinned by digest, a SIPp phone per
extension (registering with its password), a SIPp caller per call sending RFC 2833 key presses,
PostgreSQL — and proves an open transfer, a known customer transferred without a PIN, an unknown
caller with a valid PIN transferred (the note logged), a wrong PIN three times ending in the menu
hanging up, an invalid choice replaying the menu before a valid one transfers, and an expired PIN
refused — each read back from `transfer_log`, each transfer confirmed by the extension's phone.
