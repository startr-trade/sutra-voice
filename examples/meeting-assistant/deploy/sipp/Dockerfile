# The stand-in conference for the meeting-assistant example: SIPp (Alpine's maintained package)
# plus tcpdump, with the conference scenario and a recorded two-speaker conversation baked in.
#
#   conference.xml   answers the bot's call like a conference dial-in: waits while the bot
#                    enters the PIN and announces itself, plays the conversation, hangs up
#
# The conversation is synthesised at build time (make-conversation.py): two voice-like signals
# taking turns, as G.711 µ-law RTP in a pcap replayed by SIPp's play_pcap_audio. No recording of
# a real person is involved.
FROM alpine:3.22@sha256:2e0b81d60597f449baa56dc25d355367869d2dca4cd57e866f1e2b52af3eb55f
RUN apk add --no-cache sipp tcpdump python3
COPY conference.xml make-conversation.py /meeting/
RUN python3 /meeting/make-conversation.py /meeting/conversation.pcap
WORKDIR /meeting
ENTRYPOINT ["sipp"]
