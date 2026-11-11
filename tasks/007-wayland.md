# Wayland (and capture of real Linux desktops without the CPU)

- Status: under discussion (the user wants it, not now)
- Created: 2026-11-11

## Goal

Stream a Linux desktop running under Wayland (GNOME, KDE Plasma, sway,
Hyprland, gamescope…), not only X11. Today remothing captures X11 only:
real X displays and the Xvfb displays of the containers.

## Open questions

1. Which compositors first (GNOME and KDE through the portal; wlroots
   compositors have their own protocol).
2. Whether a consent dialog on the host at every start is acceptable
   (portal), or the server must run with the privileges that avoid it
   (KMS capture, `uinput`).
3. Whether the CPU-free capture paths for X11 (NvFBC on NVIDIA, KMS) are
   part of this or separate.

## What was found (2026-11-10/11, reading, nothing tried)

Capture:

- **PipeWire through the ScreenCast portal** is the standard way under
  Wayland: the user approves once (a restore token can skip the dialog
  later), frames arrive as DMA-BUFs on the GPU. Damage metadata
  (`SPA_META_VideoDamage`) is optional and not every compositor fills it.
- **wlroots** compositors also offer `wlr-screencopy` / `ext-image-copy`.
- **KMS**: reading the scanout plane's framebuffer from the kernel as a
  DMA-BUF, under any compositor; needs `CAP_SYS_ADMIN`. Sunshine does this
  (`src/platform/linux/kmsgrab.cpp`), imports it as an EGL image and
  converts to YUV in a shader straight into the encoder's surface (VAAPI)
  or CUDA memory; pixels never touch the CPU.
- **NvFBC** (NVIDIA, X11): capture straight to CUDA.

Input: Wayland has no XTEST. Either the RemoteDesktop portal (libei; the
user approves, like capture) or `uinput` (a virtual input device; needs
root or the `input` group, works under any compositor and on the console).

Clipboard: no X selections; `wl-clipboard`-style data control protocols
(`wlr-data-control`, `ext-data-control`) where the compositor has them.

HDR: only Wayland has HDR on Linux (KDE Plasma 6, GNOME, gamescope).
Screen sharing through the portal is not an HDR path (the ArchWiki lists
it as broken with HDR10 on; a KWin request for HDR screencasts dates from
July 2026). A KMS read of an HDR output is 10-bit PQ (BT.2020) with the
connector's `HDR_OUTPUT_METADATA`; the SDR white level is the compositor's
setting and not in KMS (start from 203 nits, configurable). The Windows
conversion pass and `platform::Picture` would take it (docs/ARCHITECTURE.md,
"HDR").

## What it means for us

- **The tiles stay.** X11 today tells us what changed (XDamage) and we
  read only that. KMS and NvFBC give the whole frame with no changed areas;
  PipeWire only sometimes. Without that information every frame has to be
  compared, which on Windows is done on the GPU (`gpu_diff.rs`: a compute
  shader over the tiles DXGI marks); Linux would need the same comparison
  on the GPU (CUDA or Vulkan compute) next to the capture.
- New pieces behind the platform traits (`crates/core/src/platform.rs`): a
  Wayland `DisplayProvider` / `CaptureSession` (PipeWire), an input
  provider (`uinput` or libei), a clipboard; the core, protocol and client
  stay as they are.
- Nothing changes for the containers (Xvfb has no GPU output and no
  Wayland): today's main Linux use gains nothing.

## Testing

- A real Linux machine or VM with a GPU and a Wayland session: the
  containers here cannot run a compositor with GPU output.
