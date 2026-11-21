# Building remothing

The web clients are embedded into the binary, so build them first:

```sh
cd client && npm install && npm run build && cd ..
```

## Linux

Requires FFmpeg development libraries (`libavcodec-dev libavformat-dev
libavutil-dev libswscale-dev`), `libxcb1-dev`, clang (bindgen) and, at runtime,
`Xvfb`, `setxkbmap` and an X server for real displays. Sound needs `parec`
(`pulseaudio-utils`) or `pw-record` to record the sound server in use, and
`pulseaudio` (or PipeWire with WirePlumber, pipewire-pulse and
`dbus-daemon`) where none runs, as in a container. The microphone needs
`pactl` and `pacat` (`pulseaudio-utils`), or `pw-loopback` and `pw-cat`.

```sh
cargo build --release -p remothing -p rt-testapp
```

Optional Docker deployment (simulated display + NVENC when started with `--gpus all`):

```sh
docker build -t remothing .                                  # slim: openbox + xterm
docker build --target desktop -t remothing-desktop .         # full XFCE desktop + Firefox
docker run --gpus all --shm-size=1g --cap-add SYS_NICE \
  -p 8080:8080 -p 8443:8443 -p 8080:8080/udp -p 8081:8081 -p 8081:8081/udp \
  -v remothing-tls:/etc/remothing/tls \
  -e WIDTH=2560 -e HEIGHT=1440 -e KB_LAYOUT=es -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret \
  -e WEBRTC_ADVERTISE=192.168.1.10 -e WEBTRANSPORT_ADVERTISE=192.168.1.10:8081 \
  -e TLS_LISTEN=0.0.0.0:8443 \
  remothing-desktop   # host addresses browsers reach the UDP ports at
```

`TLS_LISTEN` adds an HTTPS listener with a self-signed certificate
(`[tls]` in [USAGE.md](USAGE.md)): needed when browsers on other machines reach the container
directly, without an HTTPS proxy. The volume keeps the certificate, and so
the browsers' exception, across container rebuilds.

`--cap-add SYS_NICE` is needed in a container for the server to raise the
priority of its capture and streaming threads; without it they run at the
default priority (the log says so once) and the stream still works, with
less protection when other programs load the CPU. Outside a container the
same needs root or CAP_SYS_NICE on the binary (`setcap cap_sys_nice+ep`).

## Windows

```powershell
powershell -ExecutionPolicy Bypass -File scripts/windows-deps.ps1   # once: libclang + vcpkg static FFmpeg (with libopus)
. scripts/windows-env.ps1
cargo build --release -p remothing --features static-ffmpeg
```

## macOS

Apple Silicon, macOS 13 or later. `.github/workflows/macos.yml` builds it on
GitHub's Macs and publishes `remothing-macos-arm64` (a `.tar.gz` with the
binary). To build by hand: Xcode's command line tools, Rust, Node, and Opus
plus a static FFmpeg built as in the workflow into `~/deps`, then

```sh
export FFMPEG_DIR=~/deps MACOSX_DEPLOYMENT_TARGET=13.0 RUSTFLAGS="-C link-arg=-L$HOME/deps/lib -C link-arg=-lopus"
cargo build --release -p remothing
```

A binary downloaded with a browser carries Gatekeeper's quarantine; remove
it once with `xattr -d com.apple.quarantine remothing`.

