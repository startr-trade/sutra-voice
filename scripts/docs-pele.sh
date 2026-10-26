#!/usr/bin/env bash
# Verify the committed Pele browser build the book draws its diagrams with.
#
# Pele 1.7.0 (MIT, https://github.com/obsidianmd/pele), file dist/pele.global.js of the npm package
# `pele`, committed as docs/pele.global.js with its licence beside it as docs/pele.LICENSE. This
# repository has no node_modules, so there is nothing to rebuild from: the check compares the
# committed files with the recorded SHA-256. To bump Pele, take both files from the new npm
# package, replace them, and update the two hashes below.
set -euo pipefail
cd "$(dirname "$0")/.."

JS_SHA=4f7a9ccd52de93e4a7a60d1b1d6dd647449b7b1f8b2ccb94706b8558cec68a59
LIC_SHA=311bccc30de6f7b9cbfb64232bb0c911ccb2191abe316550813ae25b2c7925b9

[ "${1:-}" = "--check" ] || { echo "usage: $0 --check" >&2; exit 2; }
ok=0
for pair in "docs/pele.global.js:$JS_SHA" "docs/pele.LICENSE:$LIC_SHA"; do
  f=${pair%%:*}; want=${pair##*:}
  got=$(sha256sum "$f" | cut -d' ' -f1)
  if [ "$got" != "$want" ]; then echo "$f does not match the recorded SHA-256 (Pele 1.7.0)" >&2; ok=1; fi
done
[ $ok = 0 ] && echo "docs/pele.global.js is the pinned Pele 1.7.0 build (MIT; licence in docs/pele.LICENSE)"
exit $ok
