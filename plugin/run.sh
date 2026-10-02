#!/bin/sh
# Starts the taskr daemon for Herdr, detached: Herdr awaits and logs hook
# commands, so this returns at once. The daemon's lock keeps it single-instance;
# a second start exits at once.
# Herdr may not inherit install-time TASKR_INSTALL_DIR; keep the default fallback.
bin="${TASKR_INSTALL_DIR:-${HOME:-}/.local/bin}/taskr"
if [ ! -x "$bin" ]; then
	printf 'taskr plugin: %s not found; run install.sh\n' "$bin" >&2
	exit 0
fi
nohup "$bin" daemon >/dev/null 2>&1 &
exit 0
