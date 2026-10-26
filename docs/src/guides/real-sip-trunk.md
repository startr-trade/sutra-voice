# Moving to a real SIP trunk

The examples run against SIPp, which stands in for a carrier. Moving to a real trunk is
configuration of the PBX and the package; a real trunk stays a manual check, because the automated
suites do not reach a carrier. Start with the [voice-assistant example's](../examples/voice-assistant.md)
checklist and adapt it to your carrier.

## Steps

1. **The trunk.** In the PBX configuration (`deploy/rustpbx/config.toml`, section
   `[proxy.trunks.carrier]`) set `dest` to the carrier's SIP proxy, `username` and `password` to
   your credentials, and `did_numbers` to your number. Keep the `codec` list (`pcmu`, `pcma`):
   rustpbx builds an originated call's offer from it and refuses the call without.
2. **Inbound recognition.** rustpbx recognises a trunk by the **source address** of the carrier's
   signalling. Set `inbound_hosts` to the carrier's signalling addresses and nothing else.
3. **The number.** Change the route's `"to.user"` match and the `call-start` channel's
   `bind: "number:..."` in the package.
4. **The network.** Set `external_ip` to the address the carrier sends media to, publish the SIP port
   and exactly the RTP range (`rtp_start_port` to `rtp_end_port`, kept small) over UDP, and restrict
   `acl_rules`.
5. **Secrets.** Replace the development control and media tokens, use `wss://` for the control socket
   over an untrusted network (`SUTRA_VOIP_RWI_CA_FILE` for a private CA), and put the engine behind
   TLS for the media WebSocket. Secrets are references (`env:`, `file:`), never literals.
6. **The trunk size.** Set the capacity pool's slots to the carrier's concurrent-call limit.

## Registration and authentication

Some carriers authenticate by source IP, others require the PBX to **register** with a username and
password (SIP digest). Use the mechanism the carrier documents: IP authentication needs the correct
`inbound_hosts` and a stable public address; registration needs the credentials in the trunk
section and keep-alives through any NAT. Carrier credentials belong in secret references, not in a
committed file. Extension phones that register with your PBX need their own strong passwords: the
examples ship development-only ones.

## Toll fraud

A PBX exposed to the internet is a target for toll fraud: attackers register or call through it to
premium-rate and international numbers at your expense.

- Keep the SIP port reachable only from the carrier where possible, and apply `acl_rules`.
- Never leave default or example passwords on extensions or trunks.
- Restrict outbound destinations (country and premium-rate prefixes) at the PBX and at the carrier.
  Flows that dial numbers supplied by users should validate them first.
- Set a spending limit and call-rate alerts with the carrier, and use the capacity pool to cap
  concurrent calls.
- Watch call records for unusual destinations and off-hours volume.

See also [Security notes](../operations/security.md).
