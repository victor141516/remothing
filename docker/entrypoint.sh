#!/bin/bash
# Session launcher.
#   MODE=desktop  window manager + terminal (default)
#   MODE=app      window manager + the command given as arguments
#   MODE=bench    rt-testapp only (BENCH_ARGS), no window manager
set -euo pipefail

WIDTH=${WIDTH:-1920}
HEIGHT=${HEIGHT:-1080}
MODE=${MODE:-desktop}
export DISPLAY=:0

rm -f /tmp/.X0-lock /tmp/xvfb.log
Xvfb :0 -screen 0 "${WIDTH}x${HEIGHT}x24" -shmem -nolisten tcp -noreset 2>/tmp/xvfb.log &

for _ in $(seq 1 200); do
  grep -q 'shmid' /tmp/xvfb.log 2>/dev/null && xdpyinfo >/dev/null 2>&1 && break
  sleep 0.05
done
SHMID=$(grep -oP 'shmid \K[0-9]+' /tmp/xvfb.log | head -n1)
[ -n "$SHMID" ] || { cat /tmp/xvfb.log; echo "Xvfb did not report a shmid" >&2; exit 1; }

setxkbmap -layout "${KB_LAYOUT:-es}" || true

case "$MODE" in
  desktop) openbox & xterm & ;;
  app) openbox & "$@" & ;;
  bench) rt-testapp ${BENCH_ARGS:-} & ;;
  *) echo "unknown MODE $MODE" >&2; exit 1 ;;
esac

# shellcheck disable=SC2086 # SERVER_ARGS is a flag list
exec remothing-server --shmid "$SHMID" --listen 0.0.0.0:8080 --web /opt/remothing/web ${SERVER_ARGS:-}
