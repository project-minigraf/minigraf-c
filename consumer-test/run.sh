#!/usr/bin/env bash
# Consumer test: unpacks the release archive for this platform into an empty
# directory, compiles the README quick start and the consumer contract against
# it the way a user would, and runs them.
#
#   run.sh local <dist-dir> <version>   from built archives (dist-* artifacts)
#   run.sh registry <version>           from the GitHub Release v<version>
#
# On Windows it needs MSVC on PATH (cl.exe; the workflow runs msvc-dev-cmd).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO=project-minigraf/minigraf-c

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)            PLATFORM=linux-x86_64 ;;
  Linux-aarch64)           PLATFORM=linux-aarch64 ;;
  Darwin-*)                PLATFORM=macos-universal2 ;;
  MINGW*-*|MSYS*-*|CYGWIN*-*) PLATFORM=windows-x86_64 ;;
  *) echo "::error::no release archive for $(uname -s)-$(uname -m)"; exit 1 ;;
esac

# wait_for <seconds> <command...>: run the command every 30 s until it succeeds.
wait_for() {
  local limit="$1"; shift
  local start=$SECONDS
  until "$@"; do
    if (( SECONDS - start >= limit )); then
      echo "::error::gave up after ${limit}s waiting for: $*"
      return 1
    fi
    sleep 30
  done
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/archive" "$WORK/pkg" "$WORK/db"

case "${1:-}" in
  local)
    DIST="$(cd "$2" && pwd)"; VERSION="$3"
    find "$DIST" -name "minigraf-c-v$VERSION-$PLATFORM.*" -exec cp {} "$WORK/archive/" \;
    ;;
  registry)
    VERSION="$2"
    asset_ready() {
      gh release view "v$VERSION" --repo "$REPO" --json assets -q '.assets[].name' 2>/dev/null \
        | grep -qE "^minigraf-c-v$VERSION-$PLATFORM\.(tar\.gz|zip)\$"
    }
    wait_for 300 asset_ready
    gh release download "v$VERSION" --repo "$REPO" -p "minigraf-c-v$VERSION-$PLATFORM.*" -D "$WORK/archive"
    ;;
  *)
    echo "usage: $0 local <dist-dir> <version> | registry <version>" >&2
    exit 2
    ;;
esac

ARCHIVE="$(find "$WORK/archive" -type f | head -1)"
[ -n "$ARCHIVE" ] || { echo "::error::no archive minigraf-c-v$VERSION-$PLATFORM.*"; exit 1; }
echo "archive: $(basename "$ARCHIVE")"
case "$ARCHIVE" in
  *.zip) unzip -q "$ARCHIVE" -d "$WORK/pkg" ;;
  *)     tar xzf "$ARCHIVE" -C "$WORK/pkg" ;;
esac
(cd "$WORK/pkg" && find . -type f | sort)
for f in include/minigraf.h LICENSE-MIT LICENSE-APACHE; do
  [ -f "$WORK/pkg/$f" ] || { echo "::error::archive has no $f"; exit 1; }
done

cd "$WORK/pkg"
cp "$HERE/quickstart.c" "$HERE/contract.c" .
if [ "$PLATFORM" = windows-x86_64 ]; then
  [ -f minigraf.dll.lib ] || { echo "::error::archive has no minigraf.dll.lib import library"; exit 1; }
  # Dash options, not slash: Git Bash would rewrite /I... as a path.
  cl -nologo -Iinclude quickstart.c minigraf.dll.lib -Fequickstart.exe
  cl -nologo -Iinclude contract.c minigraf.dll.lib -Fecontract.exe
  DBDIR="$(cygpath -w "$WORK/db")"
else
  # The README's compile line.
  cc -o quickstart quickstart.c -Iinclude -L. -lminigraf -Wl,-rpath,.
  cc -o contract contract.c -Iinclude -L. -lminigraf -Wl,-rpath,.
  DBDIR="$WORK/db"
fi

OUT="$(./quickstart)"
echo "quickstart: $OUT"
[[ "$OUT" == '{"transacted":'* ]] || { echo "::error::quick start printed $OUT"; exit 1; }
./contract "$DBDIR"
