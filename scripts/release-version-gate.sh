#!/bin/sh
# Release gate 2: the tag names the version this tree builds.
#
#   sh scripts/release-version-gate.sh v0.6.0
#
# Run by release.yml's `tag` job, before anything is built or published. Both images are NAMED
# for the tag, so a tag that disagrees with the code would ship an image called 0.6.1 whose binary
# reports 0.6.0, and a bug report could not be tied to a build. Two versions are compared with the
# tag, and both must equal it:
#
#   * rust/Cargo.toml [workspace.package] version — the voice engine (every crate inherits it
#     with `version.workspace = true`, so a crate's own Cargo.toml would read as that literal);
#   * tools/sutra-webrtc-worker/Cargo.toml [workspace.package] version — the WebRTC worker, its
#     own Cargo workspace, released in step with the voice engine.
set -eu
cd "$(dirname "$0")/.."

tag="${1:?usage: release-version-gate.sh v<version>}"
case "$tag" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "::error::expected a v-prefixed semver tag (v0.6.0), got '$tag'" >&2; exit 1 ;;
esac
want="${tag#v}"

workspace_version() {
    awk -F'"' '/^\[workspace\.package\]/{f=1; next} /^\[/{f=0} f && /^version *= *"/{print $2; exit}' "$1"
}

fail=0
for manifest in rust/Cargo.toml tools/sutra-webrtc-worker/Cargo.toml; do
    have="$(workspace_version "$manifest")"
    if [ -z "$have" ]; then
        echo "::error::${manifest} has no [workspace.package] version" >&2
        fail=1
    elif [ "$have" != "$want" ]; then
        echo "::error::tag ${tag} names ${want}, but ${manifest} [workspace.package] version is ${have}." >&2
        fail=1
    else
        echo "${manifest}: ${have} (matches ${tag})"
    fi
done
if [ "$fail" != 0 ]; then
    echo "::error::Push the version bump, or re-tag the right commit. Nothing was published." >&2
    exit 1
fi
