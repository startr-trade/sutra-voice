# Contributing to Sutra Voice

Thanks for your interest in Sutra Voice! This document explains how to build the project, run
the tests, and submit changes.

By participating, you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Report bugs** and **request features** via GitHub Issues — please search first. Questions
  and ideas go to [Discussions](https://github.com/startr-trade/sutra-voice/discussions).
- **Improve the docs**: the [book](https://voice.sutra.startr.trade) is the mdBook under
  [`docs/`](docs/).
- **Submit code**: bug fixes, examples, and adapters for more PBXs and SFUs.

For anything large (a new PBX or SFU adapter, a change to the media-stream protocol, an
architectural change), please **open an issue to discuss it first** so we can agree on the
approach before you invest time.

**Where a change belongs.** Sutra Voice integrates with media servers; it never mixes,
transcodes or terminates media itself. Anything that is not about calls, IVR or meetings (BPMN
execution, channels, persistence, the model adapters) belongs to the
[engine repository](https://github.com/startr-trade/sutra), which this one consumes as the
`engine/` submodule. Engine fixes land there first and reach this repository through a new pin.

## Development environment

The voice crates are a Cargo workspace under [`rust/`](rust/); the engine's crates come in as
path dependencies from the `engine/` submodule. You need a stable Rust toolchain
([rustup](https://rustup.rs)), and Docker for the container tests and the WebRTC worker (which
builds against libwebrtc in its own Docker toolchain).

```bash
# After a fresh clone:
make submodule   # check out the pinned engine
make test        # tier-1: the no-Docker suite
make lint        # clippy -D warnings
```

`make help` lists every target. The test tiers:

| Tier | Command | Needs |
|---|---|---|
| 1 | `make test` | Rust toolchain only |
| 2 | `make image && make test-docker` (`P=<crate>` narrows it) | Docker: each suite runs a PBX or an SFU, simulated callers, a stub model and an object store |
| worker | `make worker-lint`, `make worker-test` | Docker |

Build the image (`make image`) before tier 2: `sutra-voice-engine:dev` is a mutable tag, and a
stale one silently tests an old binary.

## Submitting a pull request

1. **Fork** the repo and create a topic branch off `main`.
2. Keep the PR **focused** — one logical change. Small PRs get reviewed faster.
3. Before pushing, make sure it's green locally:
   ```bash
   make fmt-check
   make lint
   make test
   make audit
   ```
   and `make test-docker P=<crate>` if the change touches a transport, recording, or an example.
4. Add a **`CHANGELOG.md`** entry under `[Unreleased]` when your change is user-visible.
5. Open the PR with a clear description of **what** changed and **why**. Link the issue it
   addresses.

CI runs `fmt`, `clippy -D warnings`, the tier-1 suite, and the supply-chain audit on every PR;
the tier-2 suites run nightly.

**Test media.** Never commit a recording of a real person's voice or face without their
consent; the samples in this repository are synthetic.

## Commit messages

Use short, imperative summaries with an area prefix, e.g.:

```
feat(voip): stream per-participant meeting segments to the object store
fix(recording): resume an interrupted multipart upload after a restart
docs(guides): document record: after-consent
```

## License of contributions

Sutra Voice is dual-licensed under **MIT OR Apache-2.0**. Unless you state otherwise, any
contribution you submit is licensed under the same terms, with no additional conditions. You
retain the copyright to your contributions.
