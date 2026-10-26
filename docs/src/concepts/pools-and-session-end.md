# Session end and capacity pools

A trunk allows a fixed number of concurrent calls, shared by inbound and outbound. That limit is
declared once as an engine **capacity pool** and referenced from the channels:

```yaml
sutra:
  capacity-pools:
    trunk-main:
      slots: 30
      reserve: { inbound: 6 }      # outbound can never take the last 6
```

```yaml
  - name: call-start
    transport: voip
    bind: "number:+15551230000"
    pool: trunk-main
    pool-call-key: header.x-call-id    # one slot per call
    session-end:
      on-message: call-hangup          # the hang-up frees the line; the instance runs on
```

- A call to the number takes a slot or is refused busy.
- An `originate` waits in the outbox, oldest first with no attempt counted, until a line frees, so
  a batch of calls paces itself to the trunk.
- The slot belongs to the **call**, not the instance. `session-end` returns it at the hang-up even
  when the instance goes on with post-call work, and an end or failure returns anything still held.

The pool settings are engine configuration: see the
[configuration reference](https://sutra.startr.trade/operating/configuration.html) in the Sutra
book.
