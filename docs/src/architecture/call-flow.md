# Calls and the media protocol

## An inbound call

```mermaid
sequenceDiagram
    participant C as Caller
    participant P as PBX (rustpbx)
    participant T as voip transport
    participant F as Process instance
    participant M as llm channel
    C->>P: INVITE (SIP)
    P->>T: webhook: call_created
    T->>F: call-start message (x-call-id)
    P->>T: media WebSocket opens
    C->>P: speech (RTP)
    P->>T: PCM frames
    Note over T: energy VAD cuts a turn
    T->>F: call-turn (audio + features)
    F->>M: audio turn
    M-->>F: spoken answer
    F->>T: play (audio)
    T->>P: paced 20 ms frames
    P->>C: answer (RTP)
    C->>P: hangs up
    P->>T: webhook: call_hangup
    T->>F: call-hangup
```

The call is one process instance. After each answer it returns to the same wait, an event-based
gateway on `call-turn`, `call-dtmf`, `call-hangup` and an idle timer, so the whole conversation is
a loop in the model. The transport delivers a call's messages strictly in order and holds the next
until the instance is ready for it.

## Why playback is paced

Audio goes to the gateway in real time, 20 ms per frame, never pre-buffered. When the caller starts
speaking while an answer is playing, the transport stops sending: that is **barge-in**, and it only
works because there is nothing already queued in the PBX that could not be taken back.

## An outbound call

A flow places a call with an `voip://originate` operation on an outbound channel. The transport
asks the gateway to originate through the configured trunk, waits for the outcome and reports it
on `call-progress`; once answered, the call is handed to the engine's media bridge and from then on
looks like any other call. With a capacity pool an originate waits in the outbox until a line is
free.

The full channel and operation reference is in [The voip transport](../guides/voip-transport.md).
