#!/bin/bash
# Container launcher. The server itself creates the simulated display (Xvfb).
#   MODE=desktop  openbox + xterm in a WIDTHxHEIGHT virtual display (default)
#   MODE=app      openbox + the command given as arguments
#   MODE=bench    rt-testapp only (BENCH_ARGS), no window manager
# STREAM_TOKEN / ADMIN_TOKEN override the tokens of a freshly created config.
set -euo pipefail

CONFIG=/etc/remothing/config.toml
if [ ! -f "$CONFIG" ]; then
  mkdir -p /etc/remothing
  {
    echo '[server]'
    echo 'listen = "0.0.0.0:8080"'
    echo 'admin_listen = "0.0.0.0:8081"'
    echo '[input]'
    echo "keyboard_layout = \"${KB_LAYOUT:-es}\""
    echo '[auth]'
    echo "stream_token = \"${STREAM_TOKEN:-}\""
    [ -n "${ADMIN_TOKEN:-}" ] && echo "admin_token = \"${ADMIN_TOKEN}\""
  } > "$CONFIG"
fi

case "${MODE:-desktop}" in
  desktop) SESSION="openbox & exec xterm" ;;
  app) SESSION="openbox & exec $*" ;;
  bench) SESSION="exec rt-testapp ${BENCH_ARGS:-}" ;;
  *) echo "unknown MODE ${MODE}" >&2; exit 1 ;;
esac

exec remothing --config "$CONFIG" --virtual "${WIDTH:-1920}x${HEIGHT:-1080}" --session-command "$SESSION"
