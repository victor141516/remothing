#!/bin/bash
# Container launcher. The server itself creates the simulated display (Xvfb).
#   MODE=desktop  XFCE session (desktop image) or openbox + xterm, in a WIDTHxHEIGHT virtual display (default)
#   MODE=app      openbox + the command given as arguments
#   MODE=bench    rt-testapp only (BENCH_ARGS), no window manager
# STREAM_TOKEN / ADMIN_TOKEN override the tokens of a freshly created config.
# WEBRTC_ADVERTISE: comma-separated host addresses browsers can reach the
#   published UDP port at (container interfaces are Docker-internal); default
#   127.0.0.1 for a browser on the Docker host. Publish it with -p 8080:8080/udp.
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
    if [ -n "${ADMIN_TOKEN:-}" ]; then echo "admin_token = \"${ADMIN_TOKEN}\""; fi
    echo '[webrtc]'
    echo "advertise = [$(echo "${WEBRTC_ADVERTISE:-127.0.0.1}" | sed 's/[^,][^,]*/"&"/g')]"
  } > "$CONFIG"
fi

case "${MODE:-desktop}" in
  desktop)
    if command -v xfce4-session >/dev/null; then
      # Apps run as an unprivileged user; the server keeps root for Xvfb management.
      SESSION="exec su user -c 'HOME=/home/user dbus-launch --exit-with-session xfce4-session'"
    else
      SESSION="openbox & exec xterm"
    fi
    ;;
  app) SESSION="openbox & exec $*" ;;
  bench) SESSION="exec rt-testapp ${BENCH_ARGS:-}" ;;
  *) echo "unknown MODE ${MODE}" >&2; exit 1 ;;
esac

exec remothing --config "$CONFIG" --virtual "${WIDTH:-1920}x${HEIGHT:-1080}" --session-command "$SESSION"
