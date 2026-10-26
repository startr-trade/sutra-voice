# Overview

## The boundary: engine and voice pack

There is exactly one BPMN implementation, and it is the engine's. Nodes, gateways, events, wait
states, channels, `llm` channels and the `q:` namespace are all engine features, documented in the
[Sutra book](https://sutra.startr.trade/). Sutra Voice consumes the engine as a git submodule and
adds only what the engine's extension points allow:

| Part | What it is |
|---|---|
| `sutra-transport-voip` | The `voip` transport, registered through the engine's transport SPI: a media WebSocket on the engine's HTTP listener, turn detection, paced playback, number binding, listen-only calls, meeting sessions, segment analysis, and the rustpbx call-control client. |
| `sutra-voice-dist` | The composition root: the engine's default bundle plus `voip`, producing the `sutra-voice-engine` binary and image. |
| `tools/sutra-webrtc-worker` | A separate program, in its own workspace and image, that joins a LiveKit room and streams every participant to the engine. Nothing else links libwebrtc. |
| `examples/` | Four runnable packages and the stub model server. |

The generic seams the pack relies on stay in the engine: `llm` channels with audio input, the
`llm` channel registry, `q:audit capture="media"`, the object store, capacity pools and
`session-end`.

## Components

```mermaid
flowchart TB
    Caller[Caller phone] -->|SIP and RTP| PBX[Gateway: PBX]
    Room[Video meeting] -->|WebRTC| SFU[LiveKit SFU]
    SFU --> Worker[WebRTC worker]
    PBX -->|media stream and call control| Voip
    Worker -->|media stream| Voip
    subgraph Engine[Sutra engine image]
        Voip[voip transport] --> Flow[BPMN process instance]
        Flow --> LLM[llm channels]
        Flow --> Store[Data stores]
        Voip --> Obj[Object store: recordings]
    end
    LLM --> Model[Model provider]
```

## The media-stream protocol

The transport serves two WebSocket endpoints on the engine's own HTTP listener, with no extra
port: `/voip/bridge`, the PCM bridge a PBX dials, and `/voip/stream`, the gateway-neutral
media-stream protocol. The protocol carries linear PCM (or G.711) frames in both directions plus
small control frames, and one socket can carry a whole meeting: every participant's own audio and
sampled camera frames, tagged by participant. Any gateway that speaks it can drive the engine;
the WebRTC worker is one such gateway.

## Gateways

A gateway terminates SIP and RTP, negotiates codecs, transcodes at the edge and exchanges PCM or
G.711 with the engine. G.729, G.722 or Opus never enter Sutra.

- **rustpbx** is supported today (checked against 0.5.1). Inbound calls reach the engine through
  its IVR app in step mode; the transport takes control of the call over rustpbx's RWI socket to
  originate, transfer and hang up.
- **Generic**: the media stream is the whole interface. Call control (originate, transfer) is
  what a gateway-specific adapter such as the rustpbx one adds.
- **Asterisk and FreeSWITCH** adapters are planned, see the [Roadmap](../roadmap.md).

## The WebRTC worker

For a LiveKit room the worker joins as a visibly labelled bot, subscribes to each participant's
audio and (when requested) camera, and relays them to the engine over the media-stream protocol,
one session per meeting. See [Meetings and the WebRTC worker](meetings.md).

## Recording and the object store

A call or meeting that is recorded is written to an S3-compatible object store through the engine's
object-store layer, sealed with envelope encryption by default, capped by a maximum duration, and
**referenced by hash** from the audit trail (`q:audit capture="media"`), never embedded in it. See
[Recording and consent](../guides/recording-and-consent.md).

## Every model call is a named channel

The transport has no model settings of its own: a voip channel that names a provider, model or key
is refused at load. A responder turn, a live transcription and a post-call analysis are all `llm` channels declared in the
package, with the engine's secret references, retries, redaction and audit. Swapping a provider,
or the stub used by the tests, is a channel change and not a code change.

## Capacity

A trunk's concurrent-call limit is an engine capacity pool, shared by inbound and outbound calls,
and a call holds one slot until its hang-up. See [Session end and capacity pools](../concepts/pools-and-session-end.md).
