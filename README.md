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
docker build -t remothing .
docker run --gpus all -p 8080:8080 -p 8081:8081 -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret remothing
```
Optional Docker deployment (simulated display + NVENC when started with `--gpus all`):

```sh
docker build -t remothing .                                  # slim: openbox + xterm
docker build --target desktop -t remothing-desktop .         # full XFCE desktop + Firefox
docker run --gpus all --shm-size=1g -p 8080:8080 -p 8081:8081 \
  -e WIDTH=2560 -e HEIGHT=1440 -e KB_LAYOUT=es -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret remothing-desktop
```
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
`tuning.bandwidth_mbps`, `tuning.max_lossless_frame_kb`,
`encoder.preferred` (`auto|nvenc|amf|qsv|x264`), `input.keyboard_layout`,
`auth.*`, `tls.cert`/`tls.key`, and on Linux `[platform]`:
`x11_displays = [":0"]`, `session_command = "openbox-session"`,
`virtual_on_start = "1920x1080"`.

## Latency benchmark

Run `rt-testapp --mode idle|box|scroll|full --fps 165` fullscreen on the
streamed display (Windows: `--display \\.\DISPLAYn`) and open the client with
`?bench=10`; `window.__bench.result` holds the report described in
[docs/RESULTS.md](docs/RESULTS.md).
