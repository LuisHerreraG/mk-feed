#!/bin/bash
# Entry point for `mk-feed daemon`.
#
# Supervisors that run scripts through a shell need a script, not a
# binary. `exec` replaces this shell with the Rust process, so the supervisor's
# pid, signals and log capture belong to the daemon itself.
set -euo pipefail
exec "$HOME/.local/bin/mk-feed" daemon --interval 60
