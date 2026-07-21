#!/usr/bin/env bash
# Stop hook: run the qlty lint gate and block the turn if it fails.
#
# Exit codes are the hook contract, not qlty's:
#   0 - clean, or nothing to do (qlty absent, no config, re-entrant call)
#   2 - qlty found issues; STDERR is fed back to Claude to fix
#
# The findings must go to stderr, not stdout: on exit 2 Claude Code surfaces
# stderr as the blocking message. Writing to stdout blocks the turn with an
# empty reason, which is worse than not blocking at all.
#
# Degrades to exit 0 whenever it cannot run a meaningful check, so a
# teammate without qlty installed is never blocked from ending a turn.

set -uo pipefail

# Claude re-invokes Stop hooks after the model responds to one. Without this
# guard a failing check would loop forever.
if [ "$(jq -r '.stop_hook_active // false' 2>/dev/null)" = "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}" 2>/dev/null || exit 0

# Only gate repos that have actually adopted qlty.
[ -f .qlty/qlty.toml ] || exit 0

# Resolve qlty: PATH first, then the default install location.
QLTY=""
if command -v qlty >/dev/null 2>&1; then
  QLTY="$(command -v qlty)"
elif [ -x "$HOME/.qlty/bin/qlty" ]; then
  QLTY="$HOME/.qlty/bin/qlty"
else
  exit 0
fi

failures=""

# Plugins in comment/monitor mode still print findings but exit 0, so a
# non-zero status here means something genuinely blocking fired.
if ! output="$("$QLTY" check --all --no-progress --no-upgrade-check 2>&1)"; then
  failures="$failures"$'\n'"--- qlty check ---"$'\n'"$output"
fi

# Rust formatting is checked here rather than via qlty's rustfmt plugin: qlty
# bundles a two-year-old rustfmt that disagrees with this project's toolchain
# on 17 files. cargo fmt uses the toolchain the project actually builds with.
if command -v cargo >/dev/null 2>&1 && [ -f Cargo.toml ]; then
  if ! fmt_output="$(cargo fmt --all --check 2>&1)"; then
    failures="$failures"$'\n'"--- cargo fmt --all --check ---"$'\n'"$fmt_output"
  fi
fi

[ -z "$failures" ] && exit 0

printf 'Checks failed. Fix these before finishing:\n%s\n' "$failures" >&2
exit 2
