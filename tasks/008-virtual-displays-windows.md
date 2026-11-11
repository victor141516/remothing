# Virtual displays on Windows

- Status: under discussion (feature described; research still to do)
- Created: 2026-11-11

## Goal

What Sunshine's virtual display does, done by remothing itself on
Windows: a virtual display driver installed with remothing, and displays
created and managed from the client.

- From the display selector in the client's controls bar the user can
  create a new virtual display, choose its resolution, refresh rate and
  HDR, and connect or disconnect it (and the existing ones).
- A display created this way is a real display for Windows (programs and
  games can be moved to it and run at its resolution and refresh rate) and
  is streamed like any other.
- The driver is installed with remothing (the service already runs as
  LocalSystem).

Linux already has the equivalent for its simulated displays (Xvfb,
`VirtualDisplays` in `crates/core/src/platform.rs`: create and destroy);
the Windows feature should fit the same trait, extended where needed
(change mode, HDR, connect/disconnect).

## Open questions

All for whoever picks this up, to research and then settle with the user:

1. Which driver: our own Indirect Display driver (IddCx, a user-mode
   driver), or an existing open one. Driver signing and how it is
   installed (the service, an installer, test signing).
2. What happens to a virtual display when the client that created it
   leaves, and when the server restarts (kept, removed, remembered).
3. Whether a new client can get a display of its own size automatically,
   or only on request.
4. Which modes and refresh rates are offered, and how HDR is switched.

## Known facts

- This PC already has Sunshine's virtual display driver (SudoMaker Virtual
  Display Adapter, an IddCx driver; `\\.\DISPLAY5`, 3960x1800, while
  Sunshine had a client). Switching HDR on for it during a test on
  2026-11-11 crashed that driver (user-mode driver crash, device offline
  with code 43) together with GPU errors; it was left that way by the
  user's decision. Testing this feature must use its own driver and its
  own displays, never Sunshine's.
- The streaming side exists: DXGI capture of any output, HDR conversion,
  per-client size and frame rate (docs/ARCHITECTURE.md).
