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

# Base commit: a plain-text file plus a Python file. Each side keeps the file
# syntactically valid Python so an LSP server attached to the base pane has
# something legitimate to chew on (hover, goto-def, diagnostics, etc.).
cat > simple.txt <<'EOF'
header line
shared content here
footer line
EOF
cat > app.py <<'EOF'
"""Demo module for the helix merge smoke test."""


def greet(name: str) -> str:
    return f"Hello, {name}"


def farewell(name: str) -> str:
    return f"Goodbye, {name}"


def main() -> None:
    print(greet("world"))
    print(farewell("world"))


if __name__ == "__main__":
    main()
EOF
jj describe --quiet -m "base"
jj bookmark create --quiet -r @ base

# Five divergent sides, each tweaking both files in its own way. Merging all
# five produces a 5-way conflict on each file.
declare -A GREET=(
    [a]='return f"HELLO, {name.upper()}!"'
    [b]='return f"Hi there, {name}!!"'
    [c]='return f"Hey {name}!"'
    [d]='return f"Greetings, dear {name}"'
    [e]='return f"yo {name.lower()}"'
)
declare -A FAREWELL=(
    [a]='return f"Bye {name}, see you soon"'
    [b]='return f"Catch you later, {name}"'
    [c]='return f"Adieu, {name}"'
    [d]='return f"Farewell, dear {name}"'
    [e]='return f"peace {name.lower()}"'
)
declare -A SIMPLE=(
    [a]='Alice was here'
    [b]='Bob was here'
    [c]='Carol was here'
    [d]='Dave was here'
    [e]='Eve was here'
)

SIDES=()
for letter in a b c d e; do
    jj new --quiet base -m "side $letter"
    sed -i "s/shared content here/${SIMPLE[$letter]}/" simple.txt
    sed -i "s|return f\"Hello, {name}\"|${GREET[$letter]}|" app.py
    sed -i "s|return f\"Goodbye, {name}\"|${FAREWELL[$letter]}|" app.py
    bookmark="side_$letter"
    jj bookmark create --quiet -r @ "$bookmark"
    SIDES+=("$bookmark")
done

# Merge all five sides at once. simple.txt becomes a 5-way conflict and app.py
# has two independent 5-way conflict regions.
jj new --quiet "${SIDES[@]}" -m "merge"

echo
echo "--- jj status ---"
jj status --no-pager || true
echo "-----------------"
echo
echo "press any key to tear down."
read -rsn1
