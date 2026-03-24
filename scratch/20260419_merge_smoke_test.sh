#!/usr/bin/env bash
# Smoke-test bootstrapper for the jj merge UI in helix.
#
# Creates a throwaway jj repo with a couple of merge conflicts, prints the
# path, waits for any keypress, and removes the directory on exit.

set -euo pipefail

if ! command -v jj >/dev/null 2>&1; then
    echo "jj not found in PATH" >&2
    exit 1
fi

TMPDIR=$(mktemp -d -t helix-merge-smoke-XXXXXX)
trap 'cd /; rm -rf "$TMPDIR"' EXIT
echo "smoke-test repo: $TMPDIR"

# Redirect jj's per-user config under TMPDIR so the read-only $HOME sandbox
# doesn't surface "secure config" warnings on every jj invocation.
export XDG_CONFIG_HOME="$TMPDIR/.xdg-config"
mkdir -p "$XDG_CONFIG_HOME"

cd "$TMPDIR"

jj git init --quiet

# Base commit: two files that will diverge on each side.
cat > simple.txt <<'EOF'
header line
shared content here
footer line
EOF
cat > multi.txt <<'EOF'
line 1
zone one: original
line 3
divider
zone two: original
line 6
EOF
jj describe --quiet -m "base"
jj bookmark create --quiet -r @ base

# Side A: edit both files.
jj new --quiet -m "side A"
sed -i 's/shared content here/Alice was here/' simple.txt
sed -i 's/zone one: original/zone one: from Alice/' multi.txt
sed -i 's/zone two: original/zone two: from Alice/' multi.txt
jj bookmark create --quiet -r @ side_a

# Side B: edit both files differently.
jj new --quiet base -m "side B"
sed -i 's/shared content here/Bob was here/' simple.txt
sed -i 's/zone one: original/zone one: from Bob/' multi.txt
sed -i 's/zone two: original/zone two: from Bob/' multi.txt
jj bookmark create --quiet -r @ side_b

# Merge: simple.txt has 1 conflict region, multi.txt has 2.
jj new --quiet side_a side_b -m "merge"

echo
echo "--- jj status ---"
jj status --no-pager || true
echo "-----------------"
echo
echo "press any key to tear down."
read -rsn1
