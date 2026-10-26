# Sutra Voice

<div class="brand-lockup">
<img class="on-light" src="images/sutra-voice-lockup-light.svg" alt="Sutra Voice">
<img class="on-dark" src="images/sutra-voice-lockup-dark.svg" alt="Sutra Voice">
</div>

Sutra Voice puts phone calls, IVRs and meetings under the control of BPMN processes running on the
[Sutra](https://sutra.startr.trade/) workflow engine. A flow answers a number, hears the caller one
utterance at a time, speaks back, transfers, hangs up, places calls of its own, or sits in on a
conference or a video meeting, recording, transcribing and analysing it through the engine's `llm`
channels.

> **Preview.** Voice, meeting and video support is a preview in 0.6. The automated suites run
> against SIPp, a stub model and a local LiveKit server; real carriers, real models and real
> meeting platforms are manual checks. See [Limitations](operations/limitations.md).

## The thesis: interactions are processes

A call is not a stream to be piped between a carrier and a model. It is a business interaction with
a start, waits, decisions, retries, hand-offs and an outcome, which is exactly what a BPMN process
is. When one call is one process instance:

- the menu, the retry counter and the PIN loop are model elements you can read and change, not
  state hidden in a media server;
- every model call goes through a named [`llm` channel](https://sutra.startr.trade/building/llm-channels.html),
  so keys, retries, redaction and audit are the engine's, not code in a bot;
- recordings are written to an object store and referenced by hash in the audit trail
  (`q:audit capture="media"`), and personal data is tagged and erased by the engine's rules;
- the trunk's line limit is an engine capacity pool, shared by inbound and outbound calls.

## What it is made of

Sutra Voice does not fork the engine, and adds no BPMN element. It adds one transport (`voip`),
a composition root that links it into an engine image, a separate WebRTC media worker for LiveKit
rooms, and four runnable examples. See [Architecture](architecture/overview.md).

## What it is not

It is **not a media server**. It integrates with PBXs and SFUs and leaves RTP, SIP signalling,
codec negotiation and conference mixing to them. The engine side handles PCM16 and G.711 audio in
pure Rust, cuts turns with an energy detector and plays audio back in real time; it never mixes
streams or transcodes beyond that. It is also not a realtime speech-to-speech bridge: the flow is
the control plane, turn by turn.

## Where to go next

- Try it: [Quick start](quick-start.md).
- Understand it: [Architecture](architecture/overview.md) and [Concepts](concepts/calls-as-processes.md).
- Everything about BPMN, channels and packaging is in the [Sutra book](https://sutra.startr.trade/).

Sutra Voice is open source under MIT OR Apache-2.0, like the engine, and is maintained by Startr
Trading Technologies Private Limited. Source:
[github.com/startr-trade/sutra-voice](https://github.com/startr-trade/sutra-voice).
