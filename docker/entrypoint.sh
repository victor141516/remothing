#!/bin/bash
# Container launcher. The server itself creates the simulated display (Xvfb).
#   MODE=desktop  XFCE session (desktop image) or openbox + xterm, in a WIDTHxHEIGHT virtual display (default)
#   MODE=app      openbox + the command given as arguments
#   MODE=bench    rt-testapp only (BENCH_ARGS), no window manager
# STREAM_TOKEN / ADMIN_TOKEN override the tokens of a freshly created config.
# WEBRTC_ADVERTISE: comma-separated host addresses browsers can reach the
#   published UDP port at (container interfaces are Docker-internal); default
#   127.0.0.1 for a browser on the Docker host. Publish it with -p 8080:8080/udp.
# WEBTRANSPORT_ADVERTISE: the same for WebTransport ("host:port" of the
#   published UDP port 8081, e.g. -p 8081:8081/udp and 127.0.0.1:8081).
# TLS_LISTEN: an extra HTTPS listener with a self-signed certificate (e.g.
#   0.0.0.0:8443), for browsers that reach the server directly from another
#   machine: they decode video only in a secure context. The certificate is
#   kept in /etc/remothing/tls; mount a volume there to keep it (and the
#   browser's exception) across container rebuilds.
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
    echo '[webtransport]'
    echo "advertise = [$(echo "${WEBTRANSPORT_ADVERTISE:-}" | sed 's/[^,][^,]*/"&"/g')]"
    if [ -n "${TLS_LISTEN:-}" ]; then
      echo '[tls]'
      echo 'self_signed = true'
      echo "listen = \"${TLS_LISTEN}\""
    fi
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
