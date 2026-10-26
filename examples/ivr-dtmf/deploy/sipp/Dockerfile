# The stand-in carrier and phones for the ivr-dtmf example: SIPp (Alpine's maintained package)
# plus tcpdump and python3, with the scenarios baked in.
#
#   call.sh       a caller: dials the menu's number and presses keys (make-dtmf.py writes them
#                 as RFC 2833 telephone events into the RTP stream SIPp plays)
#   extension.sh  an extension's phone: registers with the PBX, then answers what it is handed
FROM alpine:3.22@sha256:2e0b81d60597f449baa56dc25d355367869d2dca4cd57e866f1e2b52af3eb55f
RUN apk add --no-cache sipp tcpdump python3
COPY caller.xml register.xml extension.xml make-dtmf.py call.sh extension.sh /ivr/
RUN chmod +x /ivr/call.sh /ivr/extension.sh
WORKDIR /ivr
ENTRYPOINT ["sipp"]
