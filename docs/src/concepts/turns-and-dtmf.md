# Turns, barge-in and DTMF

## Turns

The transport is turn-based. An energy-based voice activity detector cuts the caller's audio into
utterances: speech ends a turn after `silence-ms` of silence, or after `max-turn-ms`. Each turn is
delivered to the flow as one `call-turn` message with the audio and deterministic acoustic features
(duration, voiced ratio, pitch and energy contours, overtalk).

Turn detection needs a continuous media stream. A gateway that sends nothing during silence ends a
turn only when audio resumes or at `max-turn-ms`.

## Barge-in

Playback is paced in real time at 20 ms per frame. When the caller starts speaking while an answer
plays, the transport stops sending, so an interrupted answer reports `interrupted: true` and the
flow can react. A `play` can be made uninterruptible with `x-barge-in: false`.

## DTMF

Keys arrive as `call-dtmf` messages, and the flow can send keys with `voip://dtmf` (to enter a
conference PIN, for example). A flow collects digits with a loop: one wait per key until a
terminator such as `#` or a timer, with a counter for retries. Keys pressed while a prompt plays are
kept and handled in order when it ends, so a caller who knows the menu can type ahead. The `dtmf`
operation never echoes the digits back, because a PIN is a secret.

The [ivr-dtmf example](../examples/ivr-dtmf.md) is a complete menu and PIN flow built this way.
