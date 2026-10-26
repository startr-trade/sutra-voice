# Emotion analytics and compliance

Sutra Voice can analyse emotion, sentiment, engagement and expression, through the engine's `llm`
channels, for calls (from the caller's audio) and for video meetings (from each participant's audio
and sampled camera frames). Because this is the most regulated thing the project does, the defaults
are restrictive and the restrictions are enforced by the software where possible.

## Where it is decided

The voip transport analyses nothing. For a call it delivers the caller's turns and generic
acoustic features; for a meeting it writes each participant's media segment by segment and hands
the flow references. The policy, the prompt, the model call and the reading of the answer are all
in the example flows, as BPMN, DMN and FEEL that ship with the package and are audited like any
other step.

## Who is analysed

- **Customers and external participants by default.** The call examples analyse the customer's
  voice (`analyzedParties: ["customer"]`). An agent's leg never reaches the engine: a transfer hands
  the call to the PBX.
- **Staff only by explicit opt-in.** Inferring the emotions of people at work is prohibited in the
  EU (AI Act, Article 5(1)(f)), which includes contact-centre agents and colleagues in an internal
  meeting. The meeting examples decide with a decision table, `rules/analysis-policy.dmn`: an
  external party is analysed; a staff role (`staff`, `internal`, `employee`, `agent`, `host`) only
  when the deployment's switch (`allowStaff` in the video example, `internalAnalysisAllowed` in
  the dial-in one, both false as shipped) is on AND the people concerned opted in; anyone else,
  including a participant with no declared role, is not. Turn the switch on only where the
  prohibition does not apply, and document why it is lawful there.
- **Two gates in a video meeting.** The `call-segment` channel's `segment.roles` limits who is cut
  into segments at all (`[external]` in the example), so a staff participant's media is never
  even written as a segment; the flow's table then decides again for every segment.
- **Roles are declared, never inferred.** Whoever issues the room token sets the role attribute.
- **A participant who is not analysed never reaches the model.** No media of theirs is sent; the
  analysis row says `not-analysed` and why.

## What the model is asked, and what is kept

- **No identification and no sensitive attributes.** The flow's prompt names pseudonyms (`P1`,
  `P2`) and tells the model not to identify anyone or infer age, gender, ethnicity, health,
  religion, political or sexual orientation. The flow then keeps only the fields it asked for:
  anything else in the answer is dropped and listed in `removedKeys`.
- **Only what was sent counts.** A signal (voice, face, body, words) the model claims for an
  emotion but that was not sent is false, not guessed (`unsupportedClaims`); an emotion no sent
  signal supports is withheld.
- **Disagreement is reported, never averaged.** When the signals disagree (a calm voice, a tense
  posture) the row keeps the model's `disagreement`, and the report counts the segments it
  happened in.
- **Media are referenced, sealed and erasable.** Recordings are audio only. A segment's audio and
  frames are sealed objects in the recording store, named by SHA-256 in the flow's variables and
  audit events (`capture="media"`), so subject erasure and retention delete them.

## Advisory, not a decision

The analysis is data for people to review and one input among others. It must never be wired to an
automated decision about a caller or a participant, such as refusing service, scoring performance or
routing an employee's case. Keep a person between the analysis and any outcome.

## Disclosure

Tell people before they are analysed: in the greeting of a call, and in the room and the bot's
display name for a meeting. See [Recording and consent](recording-and-consent.md).

This page is engineering guidance, not legal advice. Check the rules that apply where your callers
and participants are.
