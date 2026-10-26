#!/bin/sh
# One call to the menu, as the caller: call.sh <pbx host:port> <caller number> <step>...
#
# The steps are the keys to press and the pauses between them (see make-dtmf.py), e.g.
#   call.sh pbx:5060 +15550009999 3000ms 2 4000ms 4321#
# The caller stays on the line for HOLD_SECS (default 8) after the last key, unless the PBX hangs
# up first. NUMBER overrides the number dialled.
#
# Every run gets its own SIP Call-ID. A fresh container would otherwise repeat the previous run's
# (SIPp derives it from its pid and address, which a new container reuses), and the PBX would
# answer the new INVITE as a retransmission of the last call instead of a new one.
set -eu
pbx="$1"
caller="$2"
shift 2
length_ms=$(python3 /ivr/make-dtmf.py /tmp/keys.pcap "$@")
hold_ms=$((length_ms + ${HOLD_SECS:-8} * 1000))
sed "s/HOLD_MS/${hold_ms}/" /ivr/caller.xml > /tmp/caller.xml
exec sipp -sf /tmp/caller.xml -s "${NUMBER:-+15551239000}" "$pbx" -p 5060 -mp 6000 -m 1 -nostdin \
  -key caller "$caller" -cid_str "$(cat /proc/sys/kernel/random/uuid)-%u@%s" \
  -timeout "$(( hold_ms / 1000 + 30 ))s" -timeout_error
