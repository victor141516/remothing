# Virtual displays: create them and set their resolution from the client

- Status: under discussion (analysis done 2026-11-17; nothing built)
- Created: 2026-11-11

## Goal

From the display selector in the client's controls bar, besides choosing
which display is streamed:

- create a virtual display, with a resolution (and, where the platform
  can, a refresh rate and HDR);
- change the resolution of a virtual display while it runs;
- remove it.

A virtual display is a real display for the system: programs and games
can be moved to it and run at its resolution, and it is streamed like any
other. Linux, Windows and macOS.

## Decided (2026-11-17)

- **Changing the resolution on Windows**: remove the display and add it
  again (its windows jump, a full-screen game loses its output). No driver
  change.
- **macOS**: the private `CGVirtualDisplay` API is accepted.
- **Lifetime**: virtual displays go away when the server stops.
- **"Fit my window"**: yes, a client can ask for a display of exactly its
  own size.
- **Who**: every client with the stream token.
- **HDR** on Windows virtual displays: yes, optional, chosen in the client's
  menu.
- **Testing on Windows**: first on a GitHub Windows runner, then on this PC.
- **Linux**: the main display keeps its size; the displays added from the
  client are the ones that change, by being recreated. Each simulated
  display is its own X server with its own session (`session_command`), so
  recreating one ends only that display's programs.

## Open questions

1. **Windows driver**: SudoVDA has no published build of its own; Apollo's
   installer carries one, signed with Apollo's self-signed certificate
   (`CN=sudovda@su.mk`), and that is what this PC has (driver 1.10.9.289,
   hardware ID `root\sudomaker\sudovda`, certificate in the machine's Root
   store). Use that one (installed with or taken from Apollo) or our own
   build (own hardware ID, own certificate)? See "Windows".
2. **Linux, what an added display is**: today every simulated display is a
   separate desktop (its own X server and session; windows cannot move
   between them). Is that what the user wants from "another display", or a
   second monitor of the same desktop (one X server with several outputs,
   windows moved between them), which is a different design?

## What exists today

- `VirtualDisplays` (`crates/core/src/platform.rs`): `create(width,
  height)` and `destroy(id)`, implemented on Linux only (`xvfb.rs`: one
  Xvfb per display, its framebuffer read from shared memory). The admin API
  has `POST /api/virtual` and `DELETE /api/virtual/{id}`, and the admin UI a
  form; the stream client can only choose among displays (`C_DISPLAYS`,
  `C_DISPLAY`, the selector in `ui.ts`).
- Display ids on Windows are positions (`dxgi:{adapter}:{output}`): they
  shift when a display is added or removed, so a virtual display needs an id
  that stays (its device name or monitor GUID).

## Linux

Measured 2026-11-17 in the `remothing` image:

- **Xvfb cannot change size.** Its RandR reports the start size as both
  current and maximum; `xrandr --fb` to a smaller or larger size fails
  (`BadValue` / "screen cannot be larger"), and new modes are refused,
  even when started at 3840x2160. The only way to another size is a new
  Xvfb, which ends every program of the session.
- **Xvnc (TigerVNC) can.** Started with `-rfbport -1` (no VNC listener),
  `xrandr --fb 2560x1440` and a custom 1366x777 mode both applied in place
  (screen maximum 32768x32768). It has DAMAGE, MIT-SHM, RANDR, XFIXES and
  XTEST, so it would be captured like a real X11 display (`x11.rs`: XShm +
  XDamage) instead of through Xvfb's shared-memory framebuffer. Desktops
  (XFCE) follow RandR size changes. Not measured: the cost of the XShm copies
  against reading Xvfb's framebuffer directly, and of Xvnc's own update
  tracking with no VNC client.
- Real X servers (`x11:` displays) are out of reach: virtual outputs there
  depend on the GPU driver.

## Windows

Virtual displays on Windows are Indirect Display drivers (IddCx, user mode).
The options (2026-11):

| driver | license | control | resolution change | signing | HDR |
|---|---|---|---|---|---|
| **SudoVDA** (SudoMaker, inside Apollo) | MIT / CC0 (+ Microsoft's sample) | `DeviceIoControl`: add a display at any W×H×Hz (`IOCTL_ADD_VIRTUAL_DISPLAY`, returns its adapter LUID and target id), remove it, choose the render GPU, ping | remove and add again (modes fixed when added) | none of its own: Apollo signs with a self-signed certificate it installs into Root + TrustedPublisher | yes, Windows 11 24H2 |
| **LuzVDA** (fork of SudoVDA) | MS-PL driver, MIT protocol header | same idea, new protocol (`luzvda-ioctl.h`), EDID builder | as SudoVDA | own signing | only on displays whose EDID declares it |
| **Virtual Display Driver** (VirtualDrivers, MikeTheTech) | MIT | modes in `C:\VirtualDisplayDriver\vdd_settings.xml`; changing them means a driver reload (a named-pipe interface exists in recent versions; not checked) | edit the XML and reload, which resets every virtual display | signed through SignPath (installs without importing a certificate) | 10/12-bit, Windows 11 23H2+ |
| parsec-vdd | closed, no redistribution | — | — | — | — |

Facts for building on SudoVDA (from its README and a project that chose it,
lumi-fiona/cropcast#2):

- Device interface `{e5bcc234-1e0c-418a-a0d4-ef8b7501414d}`; the INF lets
  every user open it, so after installation the server needs no admin
  rights to add or remove displays.
- A watchdog removes every virtual display if nobody pings the driver for
  3 s (configurable): displays disappear by themselves when the server dies.
- After adding: find `\\.\DISPLAYn` from the returned LUID and target id and
  apply the mode with `ChangeDisplaySettingsExW`. A fixed monitor GUID makes
  Windows remember where the display sits.
- This PC already has SudoVDA (with Sunshine, `\\.\DISPLAY5`, which must not
  be touched). Our build needs its own hardware ID and interface GUID to
  live beside it.
- Building it needs the WDK (GitHub's Windows runners have Visual Studio;
  the WDK may need installing). The certificate's private key stays out of
  the repository (GitHub secret); the service installs the public one and
  the driver (`nefcon` or `pnputil`), which is a machine-wide trust decision
  the user takes once.

## macOS

- `CGVirtualDisplay` (private CoreGraphics classes: descriptor, settings,
  mode, display) creates a display macOS treats as a monitor. It lives as
  long as the object does, so it goes away with the server, and new settings
  (another mode list) can be applied to a running display. Used by
  BetterDisplay (supports macOS 26), DeskPad and others since macOS 11.
- ScreenCaptureKit lists and captures it like any display, so the capture
  side needs nothing new. Reached from Rust through the Objective-C runtime
  (objc2) by class name, without headers.
- HDR on a virtual display: not known.
- Risk: a private API; a macOS release may change it without notice.

## Proposed design (to settle with the questions)

- **Trait**: `VirtualDisplays` grows to `create(mode)`, `set_mode(id,
  mode)`, `destroy(id)` and `capabilities()` (size limits, refresh rates
  offered, HDR, whether a mode change keeps the display), with `mode =
  {width, height, refresh_hz, hdr}`.
- **Protocol**: a `C_VIRTUAL` message with JSON (`create`, `set`, `destroy`),
  answered by the display list (`S_DISPLAYS`) with each display's kind and,
  for virtual ones, its mode. The admin API uses the same trait.
- **Client**: in the display selector, "New virtual display…" (size, refresh
  rate, HDR where offered; a "this window's size" shortcut if question 6 says
  so), and for a virtual display its size and a remove button.
- **Platforms**: Linux with Xvnc (resize in place); Windows with the SudoVDA
  fork driven by the service (ping thread, stable ids); macOS with
  `CGVirtualDisplay`.

## Testing

- Linux: the container (create, resize, remove, capture after each).
- macOS: the CI smoke test can create a virtual display and stream it.
- Windows: the driver's mechanics on a GitHub runner; anything on this PC
  only with the user's go-ahead (question 9).
