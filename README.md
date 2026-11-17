# remothing

Low-latency desktop / application streaming to a browser. Static content is
sent losslessly (pixel-exact), moving areas automatically switch to video
(AV1, HEVC or H.264, the best the browser decodes in hardware) and
are refined back to lossless when they stop. One native binary per platform:
Linux (X11), Windows, macOS (Apple Silicon).

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
docker run --gpus all --shm-size=1g --cap-add SYS_NICE \
  -p 8080:8080 -p 8443:8443 -p 8080:8080/udp -p 8081:8081 -p 8081:8081/udp \
  -v remothing-tls:/etc/remothing/tls \
  -e WIDTH=2560 -e HEIGHT=1440 -e KB_LAYOUT=es -e ADMIN_TOKEN=change-me -e STREAM_TOKEN=s3cret \
  -e WEBRTC_ADVERTISE=192.168.1.10 -e WEBTRANSPORT_ADVERTISE=192.168.1.10:8081 \
  -e TLS_LISTEN=0.0.0.0:8443 \
  remothing-desktop   # host addresses browsers reach the UDP ports at
```

`TLS_LISTEN` adds an HTTPS listener with a self-signed certificate
(`[tls]` below): needed when browsers on other machines reach the container
directly, without an HTTPS proxy. The volume keeps the certificate, and so
the browsers' exception, across container rebuilds.

`--cap-add SYS_NICE` is needed in a container for the server to raise the
priority of its capture and streaming threads; without it they run at the
default priority (the log says so once) and the stream still works, with
less protection when other programs load the CPU. Outside a container the
same needs root or CAP_SYS_NICE on the binary (`setcap cap_sys_nice+ep`).

### Windows

```powershell
powershell -ExecutionPolicy Bypass -File scripts/windows-deps.ps1   # once: libclang + vcpkg static FFmpeg (with libopus)
. scripts/windows-env.ps1
cargo build --release -p remothing --features static-ffmpeg
```

### macOS

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

## Run

```sh
remothing [--config PATH] [--display ID] [--virtual WxH] [--session-command CMD]
remothing set-admin-password PASSWORD
remothing service install|uninstall|start|stop     # Windows, as administrator; macOS, as the logged-in user
```

On first start a config file is created (Linux: `/etc/remothing/config.toml`
as root, else `~/.config/remothing/config.toml`; Windows:
`%ProgramData%\remothing\config.toml`; macOS:
`~/Library/Application Support/remothing/config.toml`) with random admin and
stream tokens.

On macOS the server needs two permissions in System Settings > Privacy &
Security, asked for on the first start: Screen & System Audio Recording
(picture and sound) and Accessibility (input; without it macOS drops the
remote input silently). Started from a terminal, they are the terminal's;
`service install` makes a launch agent that starts with the user's session,
and then they are remothing's own. Restart the server after granting them.

- Streaming client: `http://HOST:8080/?token=STREAM_TOKEN`. A remote screen
  larger than the window is scaled down to fit (`&scale=1`: 1:1 pixels);
  `&stats` shows the latency overlay. A hidden tab receives nothing until
  it is shown again, so spare tabs cost neither an encoder nor bandwidth.
  The tab at the top centre (`▾`) opens the menu, one panel in three
  groups, each item saying its state in words: Screen (`Fullscreen` —
  in Chromium over HTTPS it also captures Alt+Tab, the Windows key and
  Esc for the remote side; hold Esc to leave —, `Stats`, `Display ›`),
  Control (`Capture mouse`, `Type` on touch screens, `On-screen keyboard`,
  `Clipboard ›`: a text box where what is copied there appears and what
  is typed or pasted is copied there) and Sound (`Sound`, `Microphone`).
  `›` items open a page inside the menu, with `‹` to go back. Esc, the tab
  or a click outside closes it; that click is not sent to the remote side.
  A held key repeats on the remote side, and everything a client holds is
  released if it disconnects. Ctrl+V (Ctrl pressed first) pastes what is
  copied on this computer: the page sends its clipboard to the remote one
  before the V, over HTTPS or localhost once the browser is allowed to read
  the clipboard (Chromium asks the first time); otherwise, or with no text
  copied, it pastes the remote clipboard as it is. What is copied on the
  remote side is copied on this computer too (HTTPS or localhost, in the
  window being used); what the remote clipboard held before the page
  opened is not.
- Several displays at once: each window streams its own display. The
  `Display ›` page in the menu changes only that window, and the
  address carries it (`&display=ID`): a reload, or the address opened in
  another window, streams the same display; one that no longer exists
  gives the default display, and the address follows. The host still has
  one pointer and one keyboard: using a window moves the real cursor to
  its display. Sound plays in the newest window; `Sound` turns it on in
  the others too.
- On a phone or tablet the screen works as a trackpad: one finger moves the
  pointer, a tap clicks, a tap with two fingers is the right button, two
  fingers dragged scroll (and keep sliding when let go quickly), and a tap
  followed at once by another touch holds the button for as long as that
  touch lasts (double click, or drag). `Type` in the menu opens the
  device's own keyboard.
- HDR displays (Windows): the stream is converted to SDR relative to the
  "SDR content brightness" setting, so it looks as on the host whatever
  the slider says. When every connected client sits on an HDR screen and
  can draw HDR (Chromium-based browsers), the picture is sent in HDR
  instead; `&stats` shows `HDR`, and the admin status `hdr_max` (above 0
  while the picture is HDR).
- Games that turn the camera with the mouse: `Capture mouse` in the menu (or
  Ctrl+Alt+Shift+Z, Ctrl+Option+Shift+Z on a Mac) traps the mouse in the
  picture after a click on it, so it never leaves the window. The remote
  cursor still goes on to another display shown in another window, as a
  mouse goes from one monitor to the next (Windows, macOS, X11 outputs of
  one screen); towards a display no window shows it stops at the edge.
  The other windows draw the remote cursor meanwhile, where it is.
  Esc frees it in a window (the browser insists), and a click traps it
  again; the same shortcut or button turns capture off. A game that needs Esc: use
  `Fullscreen` too, where Esc reaches the remote side. `&capturemoves=positions`
  in the address makes the server move the remote cursor to positions,
  as without capture, instead of sending mouse movement: for comparing how
  a game reacts to each.
- Virtual displays: the `Display ›` page of the menu lists every display
  (a row shows it in this window), and `＋ New virtual display` adds one to
  the host, of the size typed in or of this window's (`Fit window`: shown
  1:1, without scaling), with a refresh rate and HDR where offered, and
  streams it at once. Every virtual display, whichever window made
  it, can be removed there (a window showing a removed one goes to the
  default display), and the one shown here can change size (Windows,
  macOS). They are displays like any other for
  the host (windows move there, programs run at their resolution) and go
  away when the server stops. Windows: they need remothing's display
  driver, which the menu offers to install when the server runs as the
  service (or as administrator); installing adds a "remothing Virtual
  Display Adapter" device and remothing's driver signing certificate to the
  computer's trusted stores (remove them in Device Manager and
  `certlm.msc`). HDR on them needs Windows 11 24H2. macOS: through a
  private macOS interface, which a macOS update may break. Linux: each one
  is an X screen of its own with its own session, at a fixed size.
- Scrolling keeps the device's steps: a mouse wheel scrolls by notches,
  a trackpad (a Mac's included, with its momentum) or two fingers on a
  touch screen scroll as finely as they move. Windows programs that read
  fine scrolling (browsers, Office…) follow it; on macOS it goes as smooth
  (pixel) scrolling, as a trackpad's; on Linux (X11) it adds up to whole
  notches.
- Games and full-screen video: when the whole screen has been moving for
  ten seconds the stream turns into plain video of everything that changes
  (motion mode; `&stats` and the admin UI show it). It sharpens to the
  exact picture after five still seconds and goes back to the usual mix
  of exact and video areas after forty quiet ones. Meanwhile a client
  whose window shows the picture smaller gets it encoded smaller when its
  link is short of room (never smaller than the window), and no client
  gets more frames per second than its screen refreshes.
- Sound: what the remote session plays is sent along (Opus) and starts with
  the first click or key in the page; `Sound` in the menu switches it
  and says whether it plays or why not (so does the last line
  of the `&stats` overlay, and the admin UI lists what the server tried).
  Windows: whatever plays on the default playback device. Linux: the
  PulseAudio or PipeWire server in use, or one started by remothing for
  the programs of its simulated displays. macOS: whatever the Mac plays,
  through ScreenCaptureKit (no driver needed).
- Microphone: `Microphone` in the menu (pages served over HTTPS or localhost)
  sends this device's microphone to the remote session, where programs
  hear it as their microphone. One client talks at a time; for the others
  the button is disabled meanwhile. Linux: a virtual source
  `remothing-mic` in the sound server. macOS: not available (it would need a
  third-party virtual audio driver). Windows: through
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
  look after a hiccup. It also lists what Windows was asked for while
  clients are connected (high process class, realtime GPU scheduling, a
  1 ms timer, the compositor as multimedia).

Main config keys: `server.listen`, `server.admin_listen`, `server.display`
(the display new windows start on; "Make default" in the admin UI sets it),
`tuning.bandwidth_mbps` (upper bound; each client's measured link decides
below it), `tuning.max_lossless_frame_kb`,
`encoder.preferred` (`auto|nvenc|amf|qsv|x264`), `encoder.codec`
(`auto`: AV1, HEVC or H.264, the first the client decodes in hardware and
the server encodes; `h264|hevc|av1`: that one, H.264 for a client without
it; a config written before codecs could be chosen says `h264`: set
`auto` to get the others),
`input.keyboard_layout`,
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
the client URL overrides the choice. `?codec=h264|hevc|av1` offers the
server only that codec (H.264 is always possible).

## Latency benchmark

Run `rt-testapp --mode idle|box|scroll|full --fps 165` fullscreen on the
streamed display (Windows: `--display \\.\DISPLAYn`) and open the client with
`?bench=10`; `window.__bench.result` holds the report described in
[docs/RESULTS.md](docs/RESULTS.md).
