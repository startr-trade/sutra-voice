#!/bin/sh
# Release gate 3: the `engine/` submodule is pinned to a released engine.
#
#   sh scripts/release-engine-pin-gate.sh v0.6.0
#   SUTRA_ENGINE_REMOTE=/path/to/engine/clone sh scripts/release-engine-pin-gate.sh v0.6.0
#
# Run by release.yml's `tag` job, before anything is built or published. The pinned commit is read
# from this commit's tree (`git ls-tree HEAD engine`, the gitlink — the submodule need not be
# checked out), and the engine's tags are read from SUTRA_ENGINE_REMOTE (default
# https://github.com/startr-trade/sutra.git) with `git ls-remote`. The gate passes only when:
#
#   1. the pinned commit is EXACTLY the commit of a `v*` tag of the engine — a commit that merely
#      contains a release, or sits between two, is not a released engine;
#   2. that engine tag is a final release when this one is (a voice v0.6.0 never ships on an
#      engine v0.6.0-rc.1);
#   3. the engine tag's MAJOR.MINOR equals this release's: versions are aligned by minor, with
#      their own patch (voice 0.6.x runs on engine 0.6.x).
set -eu
cd "$(dirname "$0")/.."

tag="${1:?usage: release-engine-pin-gate.sh v<version>}"
remote="${SUTRA_ENGINE_REMOTE:-https://github.com/startr-trade/sutra.git}"
version="${tag#v}"
minor="$(printf '%s' "$version" | cut -d. -f1,2)"

pin="$(git ls-tree HEAD engine | awk '$2 == "commit" {print $3}')"
if [ -z "$pin" ]; then
    echo "::error::this commit has no engine/ submodule entry; nothing to check the pin of." >&2
    exit 1
fi
echo "engine/ is pinned to ${pin}"

refs="$(git ls-remote --tags "$remote" 'v*')" || {
    echo "::error::could not list the engine's tags at ${remote}." >&2
    exit 1
}

# One line per tag: "<commit> <tag>". An annotated tag lists twice — the tag object, then
# `<tag>^{}` with the commit it points at — and the commit is what a pin names, so the peeled
# entry wins over the tag object.
tags="$(printf '%s\n' "$refs" | awk '
    NF == 2 {
        name = $2; sub("^refs/tags/", "", name)
        if (name ~ /\^\{\}$/) { sub(/\^\{\}$/, "", name); peeled[name] = $1 }
        else if (!(name in direct)) { direct[name] = $1; order[++n] = name }
    }
    END { for (i = 1; i <= n; i++) { t = order[i]; print ((t in peeled) ? peeled[t] : direct[t]), t } }')"

matched="$(printf '%s\n' "$tags" | awk -v pin="$pin" '$1 == pin {print $2}')"
if [ -z "$matched" ]; then
    echo "::error::the engine pin ${pin} is not a released engine: no v* tag of ${remote} names that commit." >&2
    latest="$(printf '%s\n' "$tags" | awk 'NF == 2 {print $2}' | sort -V | tail -3 | paste -sd' ' -)"
    echo "::error::the engine's latest tags are: ${latest:-<none>}. Tag the engine first, or move the pin to a tagged engine commit." >&2
    exit 1
fi

ok=""
for engine_tag in $matched; do
    engine_version="${engine_tag#v}"
    engine_minor="$(printf '%s' "$engine_version" | cut -d. -f1,2)"
    if [ "$engine_minor" != "$minor" ]; then
        echo "engine ${engine_tag} is ${engine_minor}.x; this release is ${minor}.x — not aligned"
        continue
    fi
    case "$version:$engine_version" in
        *-*:*) ;;
        *:*-*) echo "engine ${engine_tag} is a pre-release; a final release needs a final engine"; continue ;;
    esac
    ok="$engine_tag"
done
if [ -z "$ok" ]; then
    echo "::error::the engine pin ${pin} is engine $(printf '%s\n' "$matched" | paste -sd' ' -), but ${tag} needs a final engine v${minor}.x release." >&2
    exit 1
fi
echo "engine/ pins ${ok} (${pin}); ${tag} is aligned with engine ${minor}.x"
