# A call is a process instance

One call is one instance of one BPMN process. It starts when the `call-start` channel, bound to a
number (`bind: "number:+15551230000"`), receives the call, and it ends when the process reaches an
end event, which may be well after the hang-up if there is post-call work to do.

The instance holds the conversation in its variables. Every message of the call carries the call's
id in the `x-call-id` header (plus `x-caller`, `x-called` and `x-call-direction`), and the start
event declares an alias over that header so that later messages find their instance:

```xml
<q:alias name="callKey" expression="getValue(header, &quot;x-call-id&quot;)"
         unique="true" onConflict="correlate"/>
```

## Waits

Everything the call does between steps is a wait: for the caller's next turn, for a key, for the
hang-up, for an idle timer. A conversation is therefore a loop that returns to the same event-based
gateway after every answer, and each pass is a new wait with its own timer and correlation. Give
every wait of the call an arm for `call-hangup`: a message for a wait the instance is not parked at
is refused.

## Why this matters

- A retry rule is a loop with a counter in the model, visible in the diagram, not a branch in a
  bot's code.
- The same engine features apply to a call as to any process: audit, personal-data tagging and
  erasure, redaction of sensitive variables, deployment packages, hot reload.
- Post-call work (analysis, a case record, a follow-up) continues in the same instance with the
  call's data to hand.

See [Calls and the media protocol](../architecture/call-flow.md) for the message sequence.
