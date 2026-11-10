# Requirements → implementation

| req | where | status |
|---|---|---|
| R1.1 one native binary per platform, Docker optional | `crates/server` → `remothing` / `remothing.exe`; web clients embedded (rust-embed); Windows links FFmpeg and the CRT statically (only system DLLs); `Dockerfile` optional | done |
| R1.2 OS-independent core | `crates/core`: `pixels.rs` (tile diff, scroll detection), `shared.rs` (motion classification, lossless/video policy, LZ4, moves), `stream.rs` (ACK flow control, WS), `proto.rs`, `admin.rs`, `assets.rs` | done |
| R1.3 OS specifics behind traits | `crates/core/src/platform.rs`, `encode.rs`; providers in `crates/platform-linux`, `crates/platform-windows`, encoders in `crates/encoder` | done |
| R1.4 identical client and wire protocol | `client/` + `proto.rs` have no platform branches | done |
| R2.1 DisplayProvider | `DisplayProvider::{list, open, virtual_displays}`, `DisplayInfo{id,name,size,refresh,position,primary,kind}` | done |
| R2.2 CaptureSession | `FrameInfo{dirty, moved, capture_us}`, `read()` (CPU BGRX), `mapped()`, `gpu_surface()` | done |
| R2.3 InputProvider | display-relative motion, buttons, wheel v/h, evdev keys mapped per platform (XTest keycodes / PS/2 scancodes) | done |
| R2.4 CursorProvider | `cursor_change()` → shape (RGBA + hotspot) / hidden; XFixes, DXGI pointer shape | done |
| R2.5 EncoderProvider | FFmpeg H.264 low latency, IDR on demand; NVENC (CUDA / D3D11 zero copy), AMF (D3D11 / host), QuickSync (host), x264; startup probe reports unusable backends | done (AMF/QSV untested: no such GPU here) |
| R2.6 hot display switch | `DisplayControl::select` → capture loop reopens, `Shared::reconfigure` bumps the epoch → clients get HELLO + full frame + IDR | done |
| R3.1 Linux simulated display | `xvfb.rs` (Xvfb manager), `xvfb_session.rs` (shm + XDamage), XTest, XFixes | done |
| R3.2 Linux real X11 | `x11.rs`: one display per RandR output, XShm + XDamage | done |
| R3.3 Wayland | out of scope, documented in ARCHITECTURE.md | documented |
| R3.4 permissions | must reach the X server; documented | documented |
| R4.1 DXGI duplication | `dxgi.rs`: dirty + move rects, persistent D3D11 texture | done |
| R4.2 GPU encode from D3D11 texture | `crates/encoder/src/d3d11.rs` (D3D11VA frames, CopySubresourceRegion of changed rects), CPU fallback | done |
| R4.3 SendInput | `input.rs` (+ Ctrl+Alt+Del → SendSAS) | done |
| R4.4 cursor from duplication | `cursor.rs` | done |
| R4.5 Windows service (SYSTEM) | `service.rs`: install/uninstall/start/stop/run, supervisor | implemented; elevated test pending |
| R4.6 console session, secure desktops, session switch | service relaunches the server in the active console session; `desktop.rs` follows the input desktop for capture and input | implemented; elevated test pending |
| R4.7 only real outputs | DXGI outputs only, `virtual_displays()` = None | done |
| R5.1 admin UI in-process, separate port | `server.admin_listen` (default 127.0.0.1:8081), `client/admin.html` | done |
| R5.2 status | `/api/status`: platform, provider, encoders (+ unusable with reason, per-client active encoder and fallback), GPUs + utilization, CPU, clients with fps/Mbit/s/p50/p99, recent errors | done |
| R5.3 display selection, virtual create/destroy | `/api/displays`, `/api/displays/select`, `/api/virtual` | done |
| R5.4 live settings | `/api/settings` (bandwidth, lossless limit, codec, encoder preference, keyboard layout) applied without restart | done |
| R5.5 auth + TLS | admin token or salted password → session cookie / bearer; stream token on `/ws` and `/input`; rustls on both listeners | done |
| R5.6 config file | TOML (`config.rs`), written back on changes | done |
| R6.1 latency/fps targets | see RESULTS.md | see RESULTS.md |
| R6.2 same probe + harness | `rt-testapp` (X11 + Win32), client `?bench=N` | done |
| R6.3 one workspace, cfg providers | root `Cargo.toml` | done |
| R6.4 graceful degradation | encoder fallback chain with reasons (status + per client), lossless-only when no encoder, capture reopen with errors in status | done |
