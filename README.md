# remothing

Low-latency desktop / application streaming to a browser. Static content is
sent losslessly (pixel-exact), moving areas automatically switch to H.264 and
are refined back to lossless when they stop. One native binary per platform.

- Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- Measurements: [docs/RESULTS.md](docs/RESULTS.md)

## Build

The web clients are embedded into the binary, so build them first:

```sh
cd client && npm install && npm run build && cd ..
```

### Linux

Requires FFmpeg development libraries (`libavcodec-dev libavformat-dev
libavutil-dev libswscale-dev`), `libxcb1-dev`, clang (bindgen) and, at runtime,
`Xvfb`, `setxkbmap` and an X server for real displays.

```sh
cargo build --release -p remothing -p rt-testapp
```

Optional Docker deployment (simulated display + NVENC when started with `--gpus all`):

```sh
docker build -t remothing .                                  # slim: openbox + xterm
docker build --target desktop -t remothing-desktop .         # full XFCE desktop + Firefox
docker run --gpus all --shm-size=1g -p 8080:8080 -p 8080:8080/udp -p 8081:8081 \
  -e WIDTH=2560 -e HEIGHT=1440 -e KB_LAYOUT=es -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret \
  -e WEBRTC_ADVERTISE=192.168.1.10 remothing-desktop   # host addresses browsers reach the UDP port at
```

### Windows

```powershell
powershell -ExecutionPolicy Bypass -File scripts/windows-deps.ps1   # once: libclang + vcpkg static FFmpeg
. scripts/windows-env.ps1
cargo build --release -p remothing --features static-ffmpeg
```

## Run

```sh
remothing [--config PATH] [--display ID] [--virtual WxH] [--session-command CMD]
remothing set-admin-password PASSWORD
remothing service install|uninstall|start|stop     # Windows, as administrator
```

On first start a config file is created (Linux: `/etc/remothing/config.toml`
as root, else `~/.config/remothing/config.toml`; Windows:
`%ProgramData%\remothing\config.toml`) with random admin and stream tokens.

- Streaming client: `http://HOST:8080/?token=STREAM_TOKEN` (add `&stats` for the overlay)
- Admin UI: `http://127.0.0.1:8081/` (login with the admin token or password)

Main config keys: `server.listen`, `server.admin_listen`, `server.display`,
`tuning.bandwidth_mbps` (upper bound; each client's measured link decides
below it), `tuning.max_lossless_frame_kb`,
`encoder.preferred` (`auto|nvenc|amf|qsv|x264`), `input.keyboard_layout`,
`auth.*`, and on Linux `[platform]`:
`x11_displays = [":0"]`, `session_command = "openbox-session"`,
`virtual_on_start = "1920x1080"`.

`[tls]`: browsers only decode video (WebCodecs) in a secure context, so a
client reaching the server directly, not on localhost and not through an
HTTPS proxy, needs HTTPS. `cert`/`key`: PEM paths of your certificate;
`self_signed = true` instead generates one (next to the config file in
`tls/`, renewed yearly, SHA-256 fingerprint in the log; browsers warn until
it is accepted). `listen`: empty serves every listener over HTTPS; an
address (e.g. `"0.0.0.0:8443"`) adds an HTTPS listener for the client and
keeps `server.listen` and the admin UI plain, for a reverse proxy or
Tailscale Funnel in front of the plain port plus direct access.

`[webrtc]`: `enabled` (default true), `udp_port` (0 = the listen port),
`stun_servers` (public address discovery), `advertise` (extra candidate
addresses, `"ip"` or `"ip:port"`, for NAT/port forwards and containers).
When the browser reaches the server over UDP on a shorter path than the
WebSocket's (e.g. the page goes through Tailscale Funnel or a reverse proxy),
frames and input move to a WebRTC data channel; otherwise, or when UDP is
blocked or stalls, everything stays on the WebSocket. `?transport=ws` or
`?transport=webrtc` in the client URL overrides the choice.

## Latency benchmark

Run `rt-testapp --mode idle|box|scroll|full --fps 165` fullscreen on the
streamed display (Windows: `--display \\.\DISPLAYn`) and open the client with
`?bench=10`; `window.__bench.result` holds the report described in
[docs/RESULTS.md](docs/RESULTS.md).
