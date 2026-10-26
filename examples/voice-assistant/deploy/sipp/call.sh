#!/bin/sh
# One inbound call, as the caller: call.sh <pbx host:port> [extra sipp options]
#
# Every run gets its own SIP Call-ID. A fresh container would otherwise repeat the previous run's
# (SIPp derives it from its pid and address, which a new container reuses), and the PBX would
# answer the new INVITE as a retransmission of the last call instead of a new one.
pbx="$1"
shift
exec sipp -sf /voice/caller.xml -s "${NUMBER:-+15551230000}" "$pbx" -p 5060 -mp 6000 -m 1 -nostdin \
  -cid_str "$(cat /proc/sys/kernel/random/uuid)-%u@%s" -timeout 60s -timeout_error "$@"
