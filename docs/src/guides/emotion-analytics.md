# Emotion analytics and compliance

Sutra Voice can analyse emotion, sentiment, engagement and expression, through the engine's `llm`
channels, for calls (from the caller's audio) and for video meetings (from each participant's audio
and sampled camera frames). Because this is the most regulated thing the project does, the defaults
are restrictive and the restrictions are enforced by the software where possible.

## Who is analysed

- **Customers and external participants by default.** The call examples analyse the customer's
  voice (`analyzedParties: ["customer"]`). An agent's leg never reaches the engine: a transfer hands
  the call to the PBX.
- **Staff only by explicit opt-in.** Inferring the emotions of people at work is prohibited in the
  EU (AI Act, Article 5(1)(f)), which includes contact-centre agents and colleagues in an internal
  meeting. In the meeting worker a staff role listed in `analyze.roles` without
  `analyze.allow-staff: true` fails the channel at load. Enable it only where the prohibition does not
  apply and the people concerned agreed, and document why it is lawful there.
- **Roles are declared, never inferred.** Whoever issues the room token sets the role attribute.
- **A participant who is not analysed never reaches the model.** None of their audio or frames is
  sent, and their frames are not even kept.

## What the model is asked, and not asked

- **No identification and no sensitive attributes.** The model sees pseudonyms (`P1`, `P2`), is told
  not to identify anyone or infer age, gender, ethnicity, health, religion, political or sexual
  orientation, and any key carrying such an inference is stripped from its answer (listed in
  `removedKeys`).
- **Frames are not retained.** They live in memory until their segment has been analysed, and are
  never stored. Recordings are audio only, sealed, referenced by hash.

## Advisory, not a decision

The analysis is data for people to review and one input among others. It must never be wired to an
automated decision about a caller or a participant, such as refusing service, scoring performance or
routing an employee's case. Keep a person between the analysis and any outcome.

## Disclosure

Tell people before they are analysed: in the greeting of a call, and in the room and the bot's
display name for a meeting. See [Recording and consent](recording-and-consent.md).

This page is engineering guidance, not legal advice. Check the rules that apply where your callers
and participants are.
