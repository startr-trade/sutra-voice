# ivr-dtmf

A classic phone menu with no model: a recorded welcome and menu, a DMN decision table that turns a
key into an extension and an access rule, customer lookup, PIN entry with retries, transfer to PBX
extensions, and a log of every outcome (never the digits). The whole call is one instance of one
BPMN process whose menu and PIN loops are real loops.

Its walkthrough is the [Quick start](../quick-start.md), run against the real stack. It runs with rustpbx and SIPp, and
includes a table of calls that exercise every path: an open extension, a customer, a valid, an
inactive and an expired PIN, three wrong PINs, an invalid choice, and no choice at all.

The PINs in the example's store are kept in clear text, deliberately, so every path can be tried;
the README explains the production alternative (a verification service holding salted hashes).

Full README, data and commands: [examples/ivr-dtmf](https://github.com/startr-trade/sutra-voice/blob/main/examples/ivr-dtmf/README.md).
