# meeting-assistant

A bot that dials into a conference by its SIP dial-in number, enters the meeting PIN, announces
that the call is recorded and transcribed, and then only listens. The transport records the meeting
(sealed, referenced by hash in the audit trail) and transcribes it live; the flow receives only
sparse events. Afterwards it asks a model for minutes, decisions and action items, applies the
analysis policy and renders a report.

Because a dial-in is one mixed channel, speakers are not labelled by the transcript provider, and
the report says attribution is inferred. See [Mixed and per-participant media](../concepts/media-modes.md).

Full README, stub setup and live-provider notes: [examples/meeting-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/meeting-assistant/README.md).
