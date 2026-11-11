# Mouse capture for games

- Status: agreed
- Created: 2026-11-11

## Goal

Playing a 3D game through the client (the mouse turns the camera, the game
keeps its cursor in the middle of the screen) does not work today: moving
the mouse far to one side takes the pointer out of the picture and out of
the tab, and from then on it moves the client's own desktop.

The user wants a mode that traps the mouse:

- A button in the controls bar (the one with Clipboard, Sound, …) turns
  the mode on and off; **Ctrl+Alt+Shift+Z** (Ctrl+Option+Shift+Z on a Mac)
  does the same from the keyboard.
- With the mode on, a click on the picture traps the mouse: it no longer
  leaves the picture and moves the remote side as a game expects.
- With the mode off, everything is as today: the pointer moves over the
  picture, and taken out of it, it is the client's own pointer again.

## Open questions

None. Answered on 2026-11-11, see "Decided".

## Decided (user, 2026-11-11)

- **Esc**: trapping does not switch to fullscreen. In a window the browser
  always frees the mouse on Esc (nothing can prevent it), and the game does
  not get that Esc. In fullscreen with the keyboard lock (the existing
  Fullscreen button) Esc reaches the game and holding it leaves fullscreen,
  as today; the user picks fullscreen when the game needs Esc.
- **Losing the capture** some other way (Esc in a window, Alt+Tab,
  switching tabs, the browser taking it away): the mode stays on, and the
  next click on the picture traps the mouse again. Only the button or the
  shortcut turn the mode off.
- **Movement**: raw, without the client computer's pointer acceleration
  (what a game reads), where the browser offers it; with it otherwise.
- **The remote cursor** is drawn where it is while the mouse is trapped
  (on the remote desktop, outside a game): the server reports the cursor's
  position, which it does not today.

## How it works

### Browser side

The standard **Pointer Lock** API (`element.requestPointerLock()`): the
browser hides the cursor, keeps it from leaving the page and reports only
movement (`movementX`/`movementY`) instead of a position. Rules the design
follows:

- It is granted only in answer to a user action (a click, a key press), so
  the mode traps on a click on the picture; when the shortcut turns the
  mode on, it tries at once (a key press counts in Chromium) and otherwise
  waits for the click.
- `requestPointerLock({ unadjustedMovement: true })` gives raw movement
  (Chromium; to be checked per OS). Where it is refused (Firefox, Safari,
  some OSes), lock without it: the movement then carries the local
  acceleration (decided: raw when possible).
- Esc always releases the lock in a window; `pointerlockchange` tells the
  page it was lost, whatever the cause. In fullscreen with
  `navigator.keyboard.lock()` (already used by the Fullscreen button) Esc is
  delivered to the page instead.
- Touch devices have nothing to trap: the button is hidden there, like
  `Type` is hidden without a touch screen today.

### Wire

Today the client only sends absolute positions (`C_MOVE`). A game reads
relative mouse input and re-centres its cursor itself, so absolute
positions fight it. New:

- **`C_MOVE_REL`**: dx i16, dy i16, mouse counts as the lock reports them
  (fractions kept on the client and added to the next message). Sent from
  `pointerrawupdate` (as `C_MOVE` is) while the lock is held.
- **`S_CURSOR_POS`**: x u16, y u16, display-relative, sent while a client
  holds the lock and the remote cursor moved. Only to clients that asked
  (a flag in a small `C_CAPTURE` message, on/off), so nothing changes for
  the others.
- Buttons, wheel and keys stay as they are.

### Server side

- `InputProvider::motion_relative(dx, dy)`:
  - Windows: `SendInput` with `MOUSEEVENTF_MOVE` without `ABSOLUTE`. The
    cursor follows with the remote user's pointer speed and acceleration
    setting; games reading raw input get the counts as sent.
  - Linux: XTEST relative motion (`xtest_fake_input` motion with detail 1).
  - **Kept on the streamed display**: relative motion could carry the
    remote cursor onto another monitor the client cannot see. After a move
    that leaves the display's area, the provider puts the cursor back on its
    edge (Windows: an absolute move to the clamped point; a game that clips
    or re-centres the cursor never gets there).
- Cursor position: Windows DXGI duplication already reports it
  (`DXGI_OUTDUPL_FRAME_INFO.PointerPosition`); Linux reads it from X
  (`QueryPointer`, or XFixes cursor events). Next to `cursor_change` in
  `CaptureSession`, sent from the capture loop to clients in capture mode.

### Client

- `ui.ts`: a `Capture` toggle button (`on` class while the mode is on,
  tooltip naming the shortcut).
- `input.ts`: the mode flag; on a click on the picture with the mode on,
  lock; while locked, `C_MOVE_REL` instead of `C_MOVE`; `pointerlockchange`
  keeps the button's state (mode still on, mouse not trapped: a hint such
  as "click to capture" in the bar).
- The shortcut is checked in `onKey` before a key goes to the remote side:
  Z with Ctrl, Alt and Shift held is not sent; the three modifiers were
  already pressed on the remote side and are released there when the user
  lets them go, as usual.
- The remote cursor: the pointer canvas that already draws the cursor
  shape for touch draws it at the `S_CURSOR_POS` position while locked
  (hidden when the remote cursor is hidden, as a game does).
- On leaving capture (shortcut or button), the next ordinary pointer move
  sends an absolute position again: the remote cursor jumps to where the
  local one is, as when entering the picture today.

## Testing

- Pointer Lock needs a real browser window with a person at it (headless
  Chromium does not grant it reliably), and a game on the Windows machine
  is the faithful case: the user tests. Injecting relative motion into this
  PC's live session from here is not allowed (`AGENTS.md`).
- Linux, in a container: relative motion and the clamp checked with
  `xdotool getmouselocation` after `C_MOVE_REL` messages sent by a test
  page; `S_CURSOR_POS` read on the wire.
- The shortcut: a test page checks that Ctrl+Alt+Shift+Z toggles the mode
  and that no Z reaches the server.

## Not verified

- Which browsers and OSes give raw movement (`unadjustedMovement`): Chromium
  on Windows is documented to; macOS and Linux to be checked; Firefox and
  Safari do not offer it.
- Whether Safari on a Mac grants the lock as Chromium does.
- How a game that reads raw input reacts to injected relative moves at the
  client's event rate (one `SendInput` per event, 125-1000 Hz from gaming
  mice); merging moves that arrive together may be needed.
