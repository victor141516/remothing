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

## Open questions

1. **Windows driver** (see Windows): our own build of SudoVDA (or its
   fork LuzVDA) with its own device IDs and our own self-signed
   certificate, installed by the service; or Virtual Display Driver, signed
   by a trusted CA but configured through an XML file and a driver reload.
   Recommendation: SudoVDA fork.
2. **Changing the resolution on Windows**: with SudoVDA as it is, a new
   resolution means removing the display and adding it again (Windows moves
   its windows away and back; full-screen games lose their output). Extending
   the driver with a "new modes" request (`IddCxMonitorUpdateModes2`) avoids
   that, at the cost of maintaining a driver change. Which one?
3. **Linux**: replace Xvfb by Xvnc (TigerVNC, without its VNC port) for
   every simulated display, so that all of them can change size in place;
   or keep Xvfb and recreate the display (and its programs) on a resize.
   Recommendation: Xvnc, once its capture cost is measured against Xvfb's.
4. **macOS**: Apple has no public API for virtual displays; the private
   `CGVirtualDisplay` is what BetterDisplay, DeskPad and others use. Accept a
   private API that a macOS release may break?
5. **Lifetime**: virtual displays exist while the server runs and go away
   with it (all three platforms do this naturally)? Or are they remembered
   and created again at start?
6. **"Fit my window"**: should a client be able to ask for a display of
   exactly its own size (no scaling at all), or only pick sizes by hand?
7. **Who may do it**: every client with the stream token (as for choosing the
   display today), or the admin UI only (as for creating one on Linux today).
8. **HDR on virtual displays** (Windows only): offered or not. This PC's
   Sunshine virtual display crashed when HDR was switched on for it
   (2026-11-11).
9. **Testing on Windows**: the demo is the user's live desktop. Installing a
   display driver there needs the user's go-ahead; GitHub's Windows runners
   (no GPU) may do for the driver's mechanics, not for HDR or performance.

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
