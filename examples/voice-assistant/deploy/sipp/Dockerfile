# The stand-in carrier for the voice-assistant example: SIPp (Alpine's maintained package) plus
# tcpdump, with the two scenarios and a recorded utterance baked in.
#
#   caller.xml   places a call to the assistant's number and speaks once (the utterance, as RTP);
#                call.sh runs it with a fresh SIP Call-ID per run
#   carrier.xml  answers the calls the engine originates (the trunk side) and speaks once
#
# The utterance is synthesised at build time (make-utterance.py): G.711 µ-law RTP in a pcap,
# replayed by SIPp's play_pcap_audio. No recording of a real person is involved.
FROM alpine:3.22@sha256:2e0b81d60597f449baa56dc25d355367869d2dca4cd57e866f1e2b52af3eb55f
RUN apk add --no-cache sipp tcpdump python3
COPY caller.xml carrier.xml make-utterance.py call.sh /voice/
RUN python3 /voice/make-utterance.py /voice/utterance.pcap
WORKDIR /voice
ENTRYPOINT ["sipp"]
