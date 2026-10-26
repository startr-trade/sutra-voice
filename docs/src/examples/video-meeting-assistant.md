# video-meeting-assistant

A bot in a LiveKit room, through the [WebRTC worker](../guides/webrtc-worker.md). It records and
transcribes each participant's own track and analyses expression, engagement and attention, body
language and a combined emotion estimate segment by segment, flagging when the signals disagree.
Afterwards the flow stores a session record and a rendered report.

Analysis covers external participants by default; staff are opt-in only. Read
[Emotion analytics and compliance](../guides/emotion-analytics.md) before changing that. The stack
runs with a stub model and a scripted participant, no keys.

Full README, compose stack and cost notes: [examples/video-meeting-assistant](https://github.com/startr-trade/sutra-voice/blob/main/examples/video-meeting-assistant/README.md).
