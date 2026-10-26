# Roadmap

These are plans, not promises. Nothing here is committed to a date, and the order may change.

## Hosted per-participant meetings on Asterisk and FreeSWITCH

Today per-participant audio comes through the WebRTC worker and LiveKit, while a PBX conference
reaches the engine as one mixed stream. The aim is to host meetings on a PBX and receive each
participant's audio separately, with gateway adapters for both **Asterisk** and **FreeSWITCH**
alongside the existing rustpbx adapter. What each PBX can expose per participant differs, and the
plan depends on that.

## Platform connectors

Connectors that let a flow take part in meetings on hosted platforms, through the platforms' own
supported media interfaces:

- **Zoom**, via real-time media streams (RTMS).
- **Microsoft Teams**, via media bots.
- **Google Meet**, via the Meet Media API.
- **Post-meeting artifacts:** fetching the platform's own recordings and transcripts after a meeting
  for the flow to process.

Each platform has its own approval, hosting and consent requirements, and each connector would
arrive as a gateway behind the same media-stream protocol: no new BPMN element.

## Smaller items

- Video recording from the WebRTC worker to the object store.
- Phone participants through LiveKit SIP.
- Several rooms per worker.

Follow progress in the [repository](https://github.com/startr-trade/sutra-voice) and its
[changelog](https://github.com/startr-trade/sutra-voice/blob/main/CHANGELOG.md).
