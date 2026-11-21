<h1 align="center">remothing</h1>

<p align="center">
  <strong>Your desktop in a browser tab, pixel-exact.</strong>
</p>

<p align="center">
  Low-latency desktop / application streaming to a browser, from one native binary per platform.
</p>

## Sharp When Still, Video When Moving

Video streaming blurs text; lossless streaming chokes on motion. remothing sends static content
losslessly, switches moving areas to video automatically and refines them back to lossless when
they stop.

## Features

- 🔍 Pixel-exact. Static content is sent losslessly; moving areas become AV1, HEVC or H.264, the best the browser decodes in hardware.
- 🖥️ Cross-platform. One native binary for Linux (X11), Windows and macOS (Apple Silicon), with the web client embedded.
- 🪟 Multi-display. Each browser window streams its own display, and virtual displays can be added from the menu.
- 🔊 Sound. What the remote session plays is sent along (Opus), and the microphone goes the other way on Windows and Linux.
- 📋 Clipboard and files. Copy and paste work across both sides, and a file dragged onto the page lands in the remote Downloads folder.
- 📱 Touch. On a phone or tablet the screen works as a trackpad, with the device's own keyboard.
- 🚀 Adaptive transport. WebTransport datagrams first, then WebRTC or WebSocket, at a bitrate that follows each client's link.

## Use It

| What | How |
| --- | --- |
| Start the server | `remothing [--config PATH] [--display ID] [--virtual WxH] [--session-command CMD]` |
| Set an admin password | `remothing set-admin-password PASSWORD` |
| Run as a service | `remothing service install\|uninstall\|start\|stop` (Windows, as administrator; macOS, as the logged-in user) |
| Open the stream | `http://HOST:8080/?token=STREAM_TOKEN` |
| Open the admin UI | `http://127.0.0.1:8081/` (admin token or password) |
| Client menu | The `▾` tab at the top centre: Screen, Control and Sound |
| Capture the mouse (games) | `Ctrl+Alt+Shift+Z` (`Ctrl+Option+Shift+Z` on a Mac), or `Capture mouse` in the menu |
| Address options | `&stats` latency overlay, `&scale=1` 1:1 pixels, `&display=ID`, `?transport=ws\|webrtc\|wt`, `?codec=h264\|hevc\|av1` |

The first start creates a config file with random admin and stream tokens (Linux:
`/etc/remothing/config.toml` as root, else `~/.config/remothing/config.toml`; Windows:
`%ProgramData%\remothing\config.toml`; macOS: `~/Library/Application Support/remothing/config.toml`).

Full reference (client, config keys, TLS, WebTransport, WebRTC, latency benchmark):
[docs/USAGE.md](docs/USAGE.md). How it works: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
Measurements: [docs/RESULTS.md](docs/RESULTS.md).

## Install

There are no packaged releases. Build the binary for your platform (below) and run it, or use
the Docker images on Linux. The macOS workflow (`.github/workflows/macos.yml`) publishes
`remothing-macos-arm64`, a `.tar.gz` with the binary, as the run's artifact; a binary downloaded
with a browser carries Gatekeeper's quarantine, removed once with
`xattr -d com.apple.quarantine remothing`.

## Build

The web clients are embedded into the binary, so build them first:

```sh
cd client && npm install && npm run build && cd ..
```

```sh
# Linux
cargo build --release -p remothing -p rt-testapp

# Linux, Docker
docker build -t remothing .                            # slim: openbox + xterm
docker build --target desktop -t remothing-desktop .   # full XFCE desktop + Firefox
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -File scripts/windows-deps.ps1   # once: libclang + vcpkg static FFmpeg (with libopus)
. scripts/windows-env.ps1
cargo build --release -p remothing --features static-ffmpeg
```

```sh
# macOS (Apple Silicon, macOS 13 or later), with Opus and a static FFmpeg in ~/deps
export FFMPEG_DIR=~/deps MACOSX_DEPLOYMENT_TARGET=13.0 RUSTFLAGS="-C link-arg=-L$HOME/deps/lib -C link-arg=-lopus"
cargo build --release -p remothing
```

Linux packages, the `docker run` line and the macOS dependencies: [docs/BUILD.md](docs/BUILD.md).

## Development

`client/` is TypeScript (`npm run build` runs `tsc --noEmit`, then esbuild); the rest is a Cargo
workspace. On Windows, after `. scripts/windows-env.ps1`:

```powershell
cargo clippy --release --workspace --all-targets --features remothing/static-ffmpeg
cargo test --release -p remothing-core
```

The Docker build is the compile check for the Linux code, and the macOS workflow builds, tests
and smoke-tests on GitHub's Macs. Repository layout and conventions: [AGENTS.md](AGENTS.md).

## Note

Browsers only decode video (WebCodecs) in a secure context: a client that is not on localhost
and not behind an HTTPS proxy needs the `[tls]` section of the config. Linux is X11 only. On
macOS the server needs the Screen & System Audio Recording and Accessibility permissions, and
has no microphone.
