# Virtual displays on Linux: monitors of one desktop

- Status: agreed goal; how depends on issue #2 (not started)
- Created: 2026-11-11; Windows and macOS done 2026-11-17

## Goal

Virtual displays work from the client on every platform (docs/ARCHITECTURE.md,
"Virtual displays"). On Linux each one is still its own Xvfb: a separate X
screen with its own session, at a fixed size. The goal is what Windows and
macOS have: extra monitors of the same desktop (windows move between them)
whose size the client can change. The main display keeps its size.

## Decided

- Xvnc instead of Xvfb only if it costs at most 10% latency (user,
  2026-11-17). It costs more (below), so first look for a cheaper way
  (issue #2), then decide.

## Facts (measured 2026-11-17)

- Xvfb cannot change its size after starting (RandR maximum = start size,
  no new modes), takes RandR 1.5 monitors (`xrandr --setmonitor`) without an
  error but does not create them, and has several monitors of one desktop
  only as Xinerama screens given at start.
- Xvnc (TigerVNC, no VNC port) grows and shrinks its framebuffer in place
  and makes RandR 1.5 monitors (a 4480x1440 screen showed a 2560x1440 and a
  1920x1080 monitor side by side).
- Captured through `x11.rs` (XShm + XDamage), Xvnc is as fast as Xvfb
  captured the same way, but both are slower than today's Xvfb read from its
  shared-memory framebuffer: capture to draw p50 +11% (box, scroll) and +21%
  (full screen), server time +35–47%, remothing CPU +18–32%, X server CPU
  about twice. The cause is one `shm_get_image` per tile in `x11.rs::read`.
  Details and the list of things to try: issue #2.

## Next

1. Cheaper X11 capture (issue #2): one GetImage of the changed area into a
   persistent shared segment, or mapping Xvnc's framebuffer as Xvfb's is
   mapped; measure again with the same three scenes.
2. With the result, decide with the user: Xvnc, or keep Xvfb.
3. If Xvnc: `VirtualDisplays` on Linux makes RandR monitors on the session's
   X server (`resize` true), ids that stay, the capture of one monitor as a
   region of the screen.

## Testing

- The container: create, resize, remove, capture after each; the latency
  benchmark against the numbers in docs/RESULTS.md.
