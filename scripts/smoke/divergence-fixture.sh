#!/usr/bin/env bash
# Build a local file:// bare-remote fixture with a forced divergence, for
# smoke-testing push/save/rebase flows against the polydot binary.
#
# Creates a bare remote, a "helper" clone used to advance upstream, and a
# "managed" clone that polydot operates on. Also writes a minimal config.toml
# pointing at the managed clone.
#
# Usage: bash scripts/smoke/divergence-fixture.sh [clean|conflict]
#   clean     — managed adds local.txt, helper adds line to notes.txt  (rebase succeeds)
#   conflict  — both sides touch notes.txt on the same line            (rebase aborts)
#
# After running, exercise interactively:
#   ./target/debug/polydot --config $ROOT/config.toml push
#   ./target/debug/polydot --config $ROOT/config.toml save
set -e
ROOT=${POLYDOT_SMOKE_ROOT:-/tmp/polydot-smoke-divergence}
REMOTE_URL="file://$ROOT/remote.git"
SCENARIO=${1:-clean}

rm -rf "$ROOT/helper" "$ROOT/managed" "$ROOT/remote.git"
mkdir -p "$ROOT"
cd "$ROOT"

git init --bare -q -b main remote.git

git clone -q "$REMOTE_URL" helper
cd helper
git config user.email smoke@polydot.test
git config user.name  "Polydot Smoke"
echo "line 1" > notes.txt
git add notes.txt
git commit -q -m "seed"
git push -q origin main
cd ..

git clone -q "$REMOTE_URL" managed
cd managed
git config user.email smoke@polydot.test
git config user.name  "Polydot Smoke"
cd ..

case "$SCENARIO" in
  clean)
    cd "$ROOT/helper"
    echo "line 2 (from helper / upstream)" >> notes.txt
    git add notes.txt
    git commit -q -m "helper: add line 2 to notes"
    git push -q origin main

    cd "$ROOT/managed"
    echo "local-only note" > local.txt
    git add local.txt
    git commit -q -m "managed: add local.txt"
    ;;
  conflict)
    cd "$ROOT/helper"
    echo "line 1 changed by helper" > notes.txt
    git add notes.txt
    git commit -q -m "helper: rewrite notes.txt line 1"
    git push -q origin main

    cd "$ROOT/managed"
    echo "line 1 changed by managed" > notes.txt
    git add notes.txt
    git commit -q -m "managed: rewrite notes.txt line 1"
    ;;
  *)
    echo "unknown scenario: $SCENARIO" >&2
    exit 1
    ;;
esac

cat > "$ROOT/config.toml" <<EOF
[smoke]
repo  = "$REMOTE_URL"
clone = "$ROOT/managed"

[save]
default-mode = "per-repo"
EOF

echo "--- managed log ---"
git -C "$ROOT/managed" log --oneline --all --decorate
echo
echo "--- managed status ---"
git -C "$ROOT/managed" status -sb
echo
echo "config: $ROOT/config.toml"
