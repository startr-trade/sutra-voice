# Security Policy

We take the security of Sutra Voice seriously: it carries live calls, recordings and meeting
media, and it can place outbound calls that cost money. Thank you for helping keep it and its
users safe.

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues, discussions, or
pull requests.**

Instead, report privately via **GitHub's private vulnerability reporting**:

> Go to the repository's **Security** tab → **Report a vulnerability**.

If you cannot use GitHub's reporting flow, contact the maintainers privately at
**`sutra-oss-support@startr.trade`**.

Please include, as far as you can:

- A description of the vulnerability and its impact.
- Steps to reproduce (a minimal proof of concept helps).
- Affected version(s) / commit, and any relevant configuration (PBX or SFU, the transport's
  `SUTRA_VOIP_*` settings, redacted).

## What is in scope

Anything in this repository, in particular:

- **Media access**: the media-stream and bridge tokens, and anything that lets a party join,
  hear or inject audio into a call or meeting it was not admitted to.
- **Recordings and analysis segments**: anything that exposes recorded media or bypasses the
  consent rule (`record: after-consent`), sealing, or erasure.
- **Toll fraud**: anything that gets an outbound call placed past the originate allow-list,
  premium/international blocks or rate limits.
- **The WebRTC media worker** and its connection to the engine.

A vulnerability in the Sutra engine itself (BPMN execution, channels, persistence, the model
adapters) belongs to the [engine repository](https://github.com/startr-trade/sutra/security/policy).
If you are unsure which side it is on, report it here and we will route it.

## What to expect

- **Acknowledgement** within **3 business days**.
- An initial assessment and severity within **10 business days**.
- We practice **coordinated disclosure**: we'll work with you on a fix and a disclosure timeline,
  and credit you (if you wish) once a fix is released.

Please give us a reasonable window to release a fix before any public disclosure.

## Supported versions

Sutra Voice is pre-1.0, and voice, IVR and meetings ship as **preview**. Security fixes land on
`main` and in the latest release (currently `0.6.x`, which runs on engine `0.6.x`); older lines
are not patched.
