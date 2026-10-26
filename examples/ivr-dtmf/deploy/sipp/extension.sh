#!/bin/sh
# One extension's phone: extension.sh <pbx host:port> <extension> <password>
# Registers with the PBX, then answers every call it is handed (from the same SIP port, so the
# registered contact is where calls arrive).
set -eu
pbx="$1"
ext="$2"
pass="$3"
mkdir -p /capture
sipp -sf /ivr/register.xml -s "$ext" "$pbx" -p 5060 -m 1 -nostdin -au "$ext" -ap "$pass" \
  -timeout 30s -timeout_error
echo "extension $ext registered"
exec sipp -sf /ivr/extension.xml -p 5060 -mp 6000 -m 1000 -nostdin -key ext "$ext"
