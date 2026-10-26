# voice-assistant

A clinic front desk on the phone. A call to the example number is greeted, and each utterance goes
as audio to an audio-native model whose spoken answer is played back, with barge-in and key
fallbacks (`0` for an agent, `#` to end). After the call the customer's voice is analysed for
emotion and sentiment alongside the transport's acoustic metrics, a decision table works out
first-contact resolution and repeat contact, and the interaction is stored. A second flow places
appointment-reminder calls through the same capacity pool, with a retry loop.

It runs with the stub model server: no key and no phone line. A set of `*-live` variants swaps in
real models. The README also has the checklist for a real trunk and a consent section that applies
to anything that records or analyses callers.

Full README, data and commands: [examples/voice-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/voice-assistant/README.md).
