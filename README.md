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
docker run --gpus all --shm-size=1g -p 8080:8080 -p 8080:8080/udp -p 8081:8081 \
  -e WIDTH=2560 -e HEIGHT=1440 -e KB_LAYOUT=es -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret \
  -e WEBRTC_ADVERTISE=192.168.1.10 remothing-desktop   # host addresses browsers reach the UDP port at
```

### Windows

```powershell
powershell -ExecutionPolicy Bypass -File scripts/windows-deps.ps1   # once: libclang + vcpkg static FFmpeg (with libopus)
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

- Streaming client: `http://HOST:8080/?token=STREAM_TOKEN`. A remote screen
  larger than the window is scaled down to fit (`&scale=1`: 1:1 pixels);
  `&stats` shows the latency overlay. A hidden tab receives nothing until
  it is shown again, so spare tabs cost neither an encoder nor bandwidth.
  The handle at the top opens the controls: fullscreen (in Chromium over
  HTTPS it also captures Alt+Tab, the Windows key and Esc for the remote
  side; hold Esc to leave), which display is streamed, the remote
  clipboard as a text box (what is copied there appears in it, what is
  typed or pasted in it is copied there), an on-screen keyboard, and the
  stats overlay. A held key repeats on the remote side, and everything a
  client holds is released if it disconnects.
- On a phone or tablet the screen works as a trackpad: one finger moves the
  pointer, a tap clicks, a tap with two fingers is the right button, two
  fingers dragged scroll, and a tap followed at once by another touch holds
  the button for as long as that touch lasts (double click, or drag).
  `Type` in the controls opens the device's own keyboard.
- Games and full-screen video: when the whole screen has been moving for
  ten seconds the stream turns into plain video of everything that changes
  (motion mode; `&stats` and the admin UI show it). It sharpens to the
  exact picture after five still seconds and goes back to the usual mix
  of exact and video areas after forty quiet ones. Meanwhile a client
  whose window shows the picture smaller gets it encoded smaller when its
  link is short of room (never smaller than the window), and no client
  gets more frames per second than its screen refreshes.
- Sound: what the remote session plays is sent along (Opus) and starts with
  the first click or key in the page; `Sound` in the controls switches it
  and its tooltip tells whether it plays or why not (so does the last line
  of the `&stats` overlay, and the admin UI lists what the server tried).
  Windows: whatever plays on the default playback device. Linux: the
  PulseAudio or PipeWire server in use, or one started by remothing for
  the programs of its simulated displays.
- Microphone: `Mic` in the controls (pages served over HTTPS or localhost)
  sends this device's microphone to the remote session, where programs
  hear it as their microphone. One client talks at a time; for the others
  the button is disabled meanwhile. Linux: a virtual source
  `remothing-mic` in the sound server. Windows: through
  [VB-CABLE](https://www.vb-cable.com), a virtual audio cable by VB-Audio
  (donationware: free to use, donations to its author welcome). If it is
  not installed, the client offers to install it: remothing downloads the
  package from VB-Audio and runs its setup, Windows asks for administrator
  permission on the server's screen, and a restart may be needed. Programs
  record `CABLE Output`, the default microphone while a client talks.
- A stream starts at 5 Mbit/s and rises within the first second or two as
  far as the link carries, so a slow link is not flooded on connect.
- Admin UI: `http://127.0.0.1:8081/` (login with the admin token or
  password). Its status page lists what happened to each client's stream
  (path switches, estimate cuts, lost frames, stutters the client saw with
  their server (wait, upload, encode)/network/client split): the place to
  look after a hiccup.

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

`[webtransport]`: `enabled` (default true), `port` (UDP; 0 = one above
the listen port), `advertise` (more places to try: `"host"` or
`"host:port"` of a forwarded or published UDP port, or the full
`https://` URL of a proxy that ends WebTransport itself). Every client
tries WebTransport first: frames then travel as datagrams, and a lost
packet costs only its own frame a short repair instead of stalling the
stream as on TCP. It needs no certificate of its own: the server makes a
short-lived one and the client trusts it through the page (a proxy's
certificate in front, or the self-signed one accepted once). Forward or
publish its UDP port as you do for WebRTC (Docker:
`WEBTRANSPORT_ADVERTISE`); behind a proxy without UDP the page still goes
through the proxy and the datagrams straight to that port.

`[webrtc]`: `enabled` (default true), `udp_port` (0 = the listen port),
`stun_servers` (public address discovery), `advertise` (extra candidate
addresses, `"ip"` or `"ip:port"`, for NAT/port forwards and containers).
Without WebTransport, when the browser reaches the server over UDP on a
shorter path than the WebSocket's (e.g. the page goes through Tailscale
Funnel or a reverse proxy), frames and input move to a WebRTC data
channel; otherwise, or when UDP is blocked or stalls, everything stays on
the WebSocket. `?transport=ws`, `?transport=webrtc` or `?transport=wt` in
the client URL overrides the choice.

## Latency benchmark

Run `rt-testapp --mode idle|box|scroll|full --fps 165` fullscreen on the
streamed display (Windows: `--display \\.\DISPLAYn`) and open the client with
`?bench=10`; `window.__bench.result` holds the report described in
[docs/RESULTS.md](docs/RESULTS.md).
