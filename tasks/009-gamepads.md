# Gamepads

- Status: under discussion (analysis done 2026-11-12; nothing built)
- Created: 2026-11-12

## Goal

A game controller plugged into the client device works in the remote
session: the browser reads it (Gamepad API), the server makes a virtual
controller with the same state, games on the remote side see a normal
controller. Rumble the other way where the browser allows it.

## Open questions

1. **Windows backend** (see "Windows" below): ViGEmBus (installed on this
   PC, signed kernel driver, archived since 2023) or a user-mode one
   (HIDMaestro, WinUHid) that we would install with a self-signed
   certificate trusted machine-wide.
2. **Which controller the server shows**: always an Xbox 360 pad (what
   XInput games expect; the widest compatibility), or a DualShock 4 when
   the client's pad is a PlayStation one (PlayStation button prompts in
   games that support them).
3. **Rumble** in the first version or later.
4. **Several clients**: may two clients each bring pads at once (local
   multiplayer on the remote side), up to the 4 XInput slots in total?
5. **Linux target**: native hosts only, or also the containers
   (`rt-desktop`), which need extra Docker flags and a kernel module (see
   "Linux").
6. **Testing on Windows**: a virtual controller made by the demo appears
   in the user's live session (games running there see it). Only with the
   user's go-ahead, or on a session the user names.

## What the browser gives (Gamepad API)

- `navigator.getGamepads()` returns snapshots; only `gamepadconnected` /
  `gamepaddisconnected` are events. Pages poll, usually once per
  `requestAnimationFrame` (the display's rate: 120-165 Hz on the user's
  screens). It exists on the window only, not in workers: polling goes in
  `main.ts`, next to the rest of the input.
- `mapping === "standard"`: 17 buttons in a fixed order (face, bumpers,
  analog triggers as `buttons[6/7].value`, back/start, stick clicks,
  D-pad, home) and 4 axes (sticks, +Y down). This maps one to one onto an
  Xbox 360 pad. Pads without the standard mapping report other layouts;
  what to do with them is part of the design (skip and say so, or a
  best-effort table).
- Chrome and Edge show no pad until a button on it is pressed (anti
  fingerprinting); Firefox shows it on connect. Hidden tabs stop updating:
  a hidden page must release its pads (neutral state, unplug).
- Secure context only (HTTPS or localhost), which the client already needs
  for video.
- Rumble: `vibrationActuator.playEffect('dual-rumble', {duration,
  strongMagnitude, weakMagnitude})` in Chrome and Edge (`trigger-rumble` on
  Xbox pads), the older `hapticActuators[].pulse()` in Firefox, nothing in
  Safari. Effects have a duration, XInput rumble is a level that stays
  until changed: the client renews the effect while the level is not zero.
- No gyro or touchpad in the standard API: DualSense extras are out of
  reach from a browser.

Sources: MDN `GamepadHapticActuator.playEffect`; gamepad.tolerence.net
"The Gamepad API, Browser by Browser" (2026-09-30).

## Windows: how to make a virtual controller

Microsoft has no supported API for a virtual XInput pad; every option is a
third-party driver.

| option | kind | pads | rumble back | state | cost for us |
|---|---|---|---|---|---|
| **ViGEmBus** 1.22 (Nefarius) | kernel bus driver, signed by its author | Xbox 360, DualShock 4 | yes (notification) | archived 2023-11-02, no updates ever (trademark end of life); still the free fallback Sunshine uses | small: user-mode IOCTLs to `\\.\ViGEmBus`; Rust client crates exist (`vigem-client` 0.1.4, 2022) or ~a few hundred lines with the `windows` crate |
| **HIDMaestro** | UMDF2 user-mode driver, MIT, active (v2.0) | 337 profiles (Xbox 360/One/Series, DualSense, Switch Pro…), seen by XInput, DirectInput, SDL3, browsers, WGI/GameInput | yes | active, 20+ projects ship it | large: only a C# SDK; we would speak its shared-memory contract (`driver/hm_contract.h`) from Rust or host .NET; install signs the driver with a self-signed certificate put in Root and TrustedPublisher |
| **WinUHid** (cgutman, Moonlight's author) | UMDF user-mode HID framework, MIT | Xbox One, DualShock 4, DualSense, mouse (`WinUHidDevs`) | yes | active | medium: C/C++ library, own installer and certificate; how XInput sees its Xbox One pad not checked |
| libvirtualhid / Virtual HID Driver (LizardByte) | UMDF2, library MIT, Windows driver source-available | all common pads | yes | active (Aug 2026), Sunshine's main path now | paid licence per machine for Windows pads ($14.99/year or $49.99): excluded |
| `InputInjector.InjectGamepadInput` (WinRT) | built in | Xbox | no | supported | free, but reaches Windows.Gaming.Input apps only as far as known, not XInput games; not checked |

Found on this PC: ViGEmBus 1.21.442 installed and running (Sunshine's).
Its end-of-life page asks owners of 1.21.442 or older to fix the bundled
updater, which still contacts a domain that changed hands in December
2023 (it can leak the public address); not done here.

Sources: docs.nefarius.at/projects/ViGEm/End-of-Life; LizardByte,
"Introducing libvirtualhid and Virtual HID Driver" (2026-08-16);
github.com/hifihedgehog/HIDMaestro; github.com/cgutman/WinUHid;
github.com/LizardByte/libvirtualhid (alternatives table).

The service runs as LocalSystem, so opening a driver's control device is
not a permission problem on Windows.

## Linux: uinput

- A virtual Xbox 360 pad is a `uinput` device with the pad's identity
  (`045e:028e`, the `xpad` button and axis set) and `FF_RUMBLE`; rumble
  comes back on the same file descriptor (`UI_FF_UPLOAD` / `EV_FF`).
  Sunshine did this through inputtino, now libvirtualhid (Linux path free,
  `uinput` and `uhid`). Needs write access to `/dev/uinput` (root, or the
  `input` group with a udev rule).
- **Containers**, tried 2026-11-12 on Docker Desktop (WSL2 kernel
  6.18.33.2): `/dev/uinput` exists, but `evdev` and `joydev` are modules
  that were not loaded (`modprobe` in the Docker VM loads them; they are
  gone after a WSL restart). A pad made in a privileged container appeared
  in `/sys/class/input` (`event0` 13:64, `js0`, "Microsoft X-Box 360 pad")
  but **no node appeared in the container's `/dev/input`**: there is no
  udev. The server would have to create the nodes itself (`mknod`, needs
  `--device-cgroup-rule 'c 13:* rmw'` and `--device /dev/uinput`) and make
  them readable by the session user, and programs that discover pads
  through udev (SDL with udev) would still not hear of them; SDL without
  udev watches `/dev/input` with inotify. The kernel's input devices are
  shared by every container in the VM.

## Proposed design (to settle with the questions)

- **Protocol** (`proto.rs` and `proto.ts`): `C_GAMEPAD` (client pad index
  u8, flags u8 with "unplugged", buttons u16 in XInput bit order, left and
  right trigger u8, four stick axes i16): the whole state, so a lost or
  reordered message is mended by the next one. `S_RUMBLE` (pad index u8,
  strong u8, weak u8).
- **Client** (`main.ts`, new `gamepad.ts`): poll per animation frame while
  a pad is connected, send on change through the input path (input socket,
  WebRTC input channel or WebTransport input stream, as other input);
  standard mapping to XInput layout; unplug all pads when the page is
  hidden or closes; play rumble.
- **Server**: a `GamepadProvider` trait in `platform.rs` (create a pad of
  a kind, submit state, rumble out through a channel, drop = unplug); one
  backend per platform. Pads belong to the client that made them, are
  created on its first state and removed when it leaves or goes silent
  (like held keys today: `Held` in `stream.rs`), at most 4 in total.
- **Best effort**: no driver, no permission, all slots taken: the stream
  goes on, the reason reaches the log, the admin status and the client,
  with what was tried (as for sound).

## Testing

- Linux container: `evtest` / `jstest` / an SDL test program reading the
  virtual pad while a headless browser sends states (a scripted
  `C_GAMEPAD` over `/input` is enough for the server half).
- Client half: Chrome's Gamepad API cannot be faked from outside; a real
  pad on the user's machine, or a page-level stub of `getGamepads` in a
  throwaway test.
- Windows: `joy.cpl` / an XInput reader on a session the user agrees to
  (question 6).
