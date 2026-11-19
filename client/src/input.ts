import { EVDEV } from './keymap';
import { C_BUTTON, C_KEY, C_MOVE, C_MOVE_REL, C_TEXT, C_WHEEL, MOVE_AS_POSITION, WHEEL_NOTCH } from './proto';

// Browser MouseEvent.button -> X button.
const BUTTONS = [1, 2, 3, 8, 9];
/** PointerEvent.buttons bit -> MouseEvent.button. */
const BUTTON_BITS: [number, number][] = [
  [1, 0],
  [2, 2],
  [4, 1],
  [8, 3],
  [16, 4],
];
const LEFT = 1;
const RIGHT = 3;
/** Pixel delta of one wheel notch, as Chromium on Windows reports it. A
 * mouse wheel arrives in whole notches; a trackpad (and the OS's momentum
 * after a flick on one) in a few pixels at a time, which go out as
 * fractions of a notch: the remote side scrolls as finely. */
const WHEEL_STEP_PX = 100;
/** A notch scrolls three lines (`deltaMode` line, Firefox's mouse wheel). */
const LINE_UNITS = WHEEL_NOTCH / 3;
/** Two fingers dragged this far scroll one wheel step. */
const TOUCH_WHEEL_STEP_PX = 24;
/** A two-finger drag let go faster than this (px/ms) keeps scrolling, slowing
 * down by e every FLING_DECAY_MS, as a phone's own pages do; below
 * FLING_STOP it ends. Only the last FLING_WINDOW_MS of the drag count. */
const FLING_MIN = 0.3;
const FLING_STOP = 0.02;
const FLING_DECAY_MS = 325;
const FLING_WINDOW_MS = 100;
/** A finger never rests perfectly still: fingers that travel less than this, all together, made a tap. */
const TAP_SLOP_PX = 10;
/** A touch this soon after a tap holds the left button: the second click of a double click, or a drag. */
const DOUBLE_TAP_MS = 300;
/** Longest wait for the server to confirm a paste before held-back keys go out anyway. */
const PASTE_WAIT_MS = 2000;

export class Input {
  private keys = new Set<number>();
  private buttons = new Set<number>();
  /** Wheel units (1/WHEEL_NOTCH of a notch) not sent yet (less than one). */
  private wheelX = 0;
  private wheelY = 0;
  /** Recent two-finger scroll moves (finger pixels), for the fling. */
  private scrollMoves: { t: number; dx: number; dy: number }[] = [];
  /** The running fling's animation frame. */
  private fling = 0;
  /** Fingers on the screen, with their last position. */
  private touches = new Map<number, { x: number; y: number }>();
  /** The touch gesture going on: most fingers down at once, and how far they travelled. */
  private fingers = 0;
  private travel = 0;
  /** When the last left tap ended, and whether this touch holds the left button because it followed one. */
  private tappedAt = -Infinity;
  private dragging = false;
  /** Remote screen size; the canvas itself belongs to the render worker. */
  screenWidth = 0;
  screenHeight = 0;
  /** Where this client last put the remote pointer (remote pixels). */
  pointerX = 0;
  pointerY = 0;
  /** The pointer is driven by touch: the browser shows no cursor for it. */
  touch = false;
  /** Capture mode (games): a click on the picture traps the mouse, which then moves the remote side by counts. */
  capture = false;
  /** Mouse counts not sent yet (less than one). */
  private relX = 0;
  private relY = 0;
  /** Buttons still held from the click that trapped the mouse (not sent). */
  private ignored = 0;
  /** While the mouse is trapped, the server injects its counts as positions
   * (`?capturemoves=positions`), as without capture: for comparing how a
   * program takes either. */
  movesAsPositions = false;
  /** A Ctrl+V waiting for this computer's clipboard to reach the remote one (`startPaste`). */
  private paste: { done: number } | null = null;
  /** Keys pressed or released meanwhile, sent once it is done. */
  private queued: [number, boolean][] = [];
  /** The V of that paste is still held. */
  private pasteV = false;

  /** `pointerChanged`: `touch` changed, or the pointer moved by touch.
   * `captureChanged`: the capture mode, or whether the mouse is trapped, changed. */
  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly send: (msg: Uint8Array) => void,
    /** This computer's clipboard text for a Ctrl+V; the server sets it and presses V. */
    private readonly sendPaste: (text: string) => void,
    private readonly pointerChanged: () => void,
    private readonly captureChanged: () => void,
  ) {
    // pointerrawupdate fires as soon as the OS delivers motion, not aligned to frames.
    const moveEvent = 'onpointerrawupdate' in window ? 'pointerrawupdate' : 'pointermove';
    // A mouse or pen points at a place of the picture.
    canvas.addEventListener(moveEvent, (e) => this.onMove(e as PointerEvent));
    // A button pressed or let go while another is held comes as a move, not
    // as pointerdown/pointerup ("chorded" buttons), and maybe not as a raw update.
    if (moveEvent !== 'pointermove') {
      canvas.addEventListener('pointermove', (e) => {
        if (e.pointerType !== 'touch') this.syncButtons(e.buttons);
      });
    }
    canvas.addEventListener('pointerdown', (e) => {
      if (e.pointerType === 'touch') return;
      canvas.focus();
      e.preventDefault();
      if (this.capture && !this.locked) {
        // This click only traps the mouse; the remote side does not get it.
        this.ignored = e.buttons;
        this.lock();
        return;
      }
      if (!this.locked) {
        canvas.setPointerCapture(e.pointerId);
        this.onMove(e);
      }
      this.button(e.button, true);
    });
    canvas.addEventListener('pointerup', (e) => {
      if (e.pointerType === 'touch') return;
      if (!this.locked) this.onMove(e);
      this.button(e.button, false);
      e.preventDefault();
    });
    // Lost (Esc in a window, Alt+Tab, another tab) or taken: the mode stays,
    // the next click on the picture traps the mouse again.
    document.addEventListener('pointerlockchange', () => this.captureChanged());
    // Touch works as on a trackpad, and so anywhere around the picture too
    // (a wide screen on an upright phone is a narrow strip): one finger
    // moves the pointer from where it is, a tap clicks, a tap with two
    // fingers is the right button, two fingers dragged scroll, and a touch
    // right after a tap holds the left button while it lasts (drag).
    const body = document.body;
    body.addEventListener('pointerdown', (e) => {
      if (e.pointerType !== 'touch' || (e.target !== canvas && e.target !== body)) return;
      // No focus change (a phone's keyboard would close) and no mouse events
      // made up from the touch; a keyboard attached to the device types into
      // the picture unless a field has the focus.
      e.preventDefault();
      if (document.activeElement === body) canvas.focus({ preventScroll: true });
      this.touchDown(e);
    });
    body.addEventListener(moveEvent, (e) => this.touchMove(e as PointerEvent));
    body.addEventListener('pointerup', (e) => this.touchUp(e, true));
    body.addEventListener('pointercancel', (e) => this.touchUp(e, false));
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());
    canvas.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    window.addEventListener('keydown', (e) => this.onKey(e, true));
    window.addEventListener('keyup', (e) => this.onKey(e, false));
    window.addEventListener('blur', () => this.releaseAll());
    canvas.addEventListener('blur', () => this.releaseAll());
    // A click anywhere but the picture (the menu, the area around it) takes
    // the keyboard away from it. A touch around the picture is the trackpad.
    window.addEventListener(
      'pointerdown',
      (e) => {
        const trackpad = e.pointerType === 'touch' && e.target === body;
        if (e.target !== canvas && !trackpad && document.activeElement === canvas) canvas.blur();
      },
      true,
    );
  }

  /** The remote screen, as a HELLO describes it. */
  setScreen(width: number, height: number): void {
    if (width !== this.screenWidth || height !== this.screenHeight) {
      // Another screen: touch starts from its middle.
      this.pointerX = width / 2;
      this.pointerY = height / 2;
    }
    this.screenWidth = width;
    this.screenHeight = height;
  }

  /** The mouse is trapped in the picture (Pointer Lock). */
  get locked(): boolean {
    return document.pointerLockElement === this.canvas;
  }

  /** Turns capture mode on (trying to trap the mouse at once: a key press or
   * button click counts as the user's gesture) or off. */
  setCapture(on: boolean): void {
    this.capture = on;
    if (on) this.lock();
    else if (this.locked) document.exitPointerLock();
    this.captureChanged();
  }

  /** Raw movement (without this computer's pointer acceleration, what a game
   * reads) where the browser offers it, else as the pointer moves. */
  private lock(): void {
    const canvas = this.canvas;
    void Promise.resolve(canvas.requestPointerLock({ unadjustedMovement: true }))
      .catch(() => canvas.requestPointerLock())
      .catch(() => {});
  }

  moveTo(x: number, y: number): void {
    const m = new Uint8Array(6);
    const v = new DataView(m.buffer);
    m[0] = C_MOVE;
    v.setUint16(2, x, true);
    v.setUint16(4, y, true);
    this.send(m);
  }

  private onMove(e: PointerEvent): void {
    if (this.screenWidth === 0 || e.pointerType === 'touch') return;
    if (this.locked) {
      this.moveBy(e.movementX, e.movementY);
      this.syncButtons(e.buttons);
      return;
    }
    const r = this.canvas.getBoundingClientRect();
    const x = Math.round(((e.clientX - r.left) * this.screenWidth) / r.width);
    const y = Math.round(((e.clientY - r.top) * this.screenHeight) / r.height);
    this.pointerX = Math.max(0, Math.min(this.screenWidth - 1, x));
    this.pointerY = Math.max(0, Math.min(this.screenHeight - 1, y));
    this.moveTo(this.pointerX, this.pointerY);
    this.syncButtons(e.buttons);
    if (this.touch) {
      this.touch = false;
      this.pointerChanged();
    }
  }

  /** Moves the remote pointer by mouse counts (fractions add up). */
  private moveBy(dx: number, dy: number): void {
    this.relX += dx;
    this.relY += dy;
    const sx = Math.max(-32767, Math.min(32767, Math.trunc(this.relX)));
    const sy = Math.max(-32767, Math.min(32767, Math.trunc(this.relY)));
    if (sx === 0 && sy === 0) return;
    this.relX -= sx;
    this.relY -= sy;
    const m = new Uint8Array(6);
    const v = new DataView(m.buffer);
    m[0] = C_MOVE_REL;
    m[1] = this.movesAsPositions ? MOVE_AS_POSITION : 0;
    v.setInt16(2, sx, true);
    v.setInt16(4, sy, true);
    this.send(m);
  }

  private button(browserButton: number, down: boolean): void {
    const b = BUTTONS[browserButton];
    if (b === undefined) return;
    if (down) {
      if (this.buttons.has(b)) return;
      this.buttons.add(b);
    } else if (!this.buttons.delete(b)) return;
    this.send(new Uint8Array([C_BUTTON, b, down ? 1 : 0]));
  }

  /** Held buttons follow the event's `buttons`, so chorded presses and
   * releases are sent too. */
  private syncButtons(buttons: number): void {
    this.ignored &= buttons;
    const pressed = buttons & ~this.ignored;
    for (const [bit, browserButton] of BUTTON_BITS) this.button(browserButton, (pressed & bit) !== 0);
  }

  private touchDown(e: PointerEvent): void {
    if (this.touches.size === 0) {
      cancelAnimationFrame(this.fling);
      this.scrollMoves = [];
      this.fingers = 0;
      this.travel = 0;
      if (performance.now() - this.tappedAt < DOUBLE_TAP_MS && this.screenWidth > 0) {
        this.dragging = true;
        this.moveTo(Math.round(this.pointerX), Math.round(this.pointerY));
        this.buttons.add(LEFT);
        this.send(new Uint8Array([C_BUTTON, LEFT, 1]));
      }
    }
    this.touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
    this.fingers = Math.max(this.fingers, this.touches.size);
    if (!this.touch) {
      this.touch = true;
      this.pointerChanged();
    }
  }

  private touchMove(e: PointerEvent): void {
    const last = this.touches.get(e.pointerId);
    if (!last || this.screenWidth === 0) return;
    const dx = e.clientX - last.x;
    const dy = e.clientY - last.y;
    last.x = e.clientX;
    last.y = e.clientY;
    this.travel += Math.hypot(dx, dy);
    if (this.travel < TAP_SLOP_PX) return;
    if (this.dragging ? this.touches.size === 1 : this.fingers === 1) {
      // As far on the remote screen as the finger went over its picture.
      const scale = this.screenWidth / this.canvas.getBoundingClientRect().width;
      this.pointerX = Math.max(0, Math.min(this.screenWidth - 1, this.pointerX + dx * scale));
      this.pointerY = Math.max(0, Math.min(this.screenHeight - 1, this.pointerY + dy * scale));
      this.moveTo(Math.round(this.pointerX), Math.round(this.pointerY));
      this.pointerChanged();
    } else if (!this.dragging && this.touches.size === 2) {
      // Each finger reports its half of the drag. What is on the screen
      // follows the fingers, as when scrolling the phone's own pages.
      this.touchScroll(-dx / 2, -dy / 2);
    }
  }

  /** A finger left the screen, or (`completed` false) the browser took the touch away. */
  private touchUp(e: PointerEvent, completed: boolean): void {
    if (!this.touches.delete(e.pointerId)) return;
    if (!completed) this.travel = Infinity;
    if (this.touches.size > 0) return;
    const tapped = this.travel < TAP_SLOP_PX && this.screenWidth > 0;
    this.tappedAt = -Infinity;
    if (this.dragging) {
      this.dragging = false;
      if (this.buttons.delete(LEFT)) this.send(new Uint8Array([C_BUTTON, LEFT, 0]));
      // Without moving it was a click: one more touch right away continues the series.
      if (tapped) this.tappedAt = performance.now();
      return;
    }
    const button = this.fingers === 1 ? LEFT : this.fingers === 2 ? RIGHT : undefined;
    if (!tapped && this.fingers === 2) this.startFling();
    if (!tapped || button === undefined) return;
    // The click goes where the pointer is drawn, even if something else moved the remote one since.
    this.moveTo(Math.round(this.pointerX), Math.round(this.pointerY));
    this.send(new Uint8Array([C_BUTTON, button, 1]));
    this.send(new Uint8Array([C_BUTTON, button, 0]));
    if (button === LEFT) this.tappedAt = performance.now();
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    const units =
      e.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? LINE_UNITS
        : e.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? 4 * WHEEL_NOTCH
          : WHEEL_NOTCH / WHEEL_STEP_PX;
    this.scroll(e.deltaX * units, e.deltaY * units);
  }

  /** Scrolls by finger pixels of a two-finger drag. */
  private touchScroll(dx: number, dy: number): void {
    const t = performance.now();
    this.scrollMoves.push({ t, dx, dy });
    while (this.scrollMoves[0].t < t - FLING_WINDOW_MS) this.scrollMoves.shift();
    this.scroll((dx * WHEEL_NOTCH) / TOUCH_WHEEL_STEP_PX, (dy * WHEEL_NOTCH) / TOUCH_WHEEL_STEP_PX);
  }

  /** Keeps a two-finger drag that was let go fast scrolling, slower and slower. */
  private startFling(): void {
    const now = performance.now();
    const recent = this.scrollMoves.filter((m) => m.t >= now - FLING_WINDOW_MS);
    this.scrollMoves = [];
    if (recent.length < 2) return;
    const span = Math.max(now - recent[0].t, 1);
    let vx = recent.reduce((s, m) => s + m.dx, 0) / span;
    let vy = recent.reduce((s, m) => s + m.dy, 0) / span;
    if (Math.hypot(vx, vy) < FLING_MIN) return;
    let last = now;
    const step = (t: number): void => {
      const dt = Math.max(t - last, 0);
      last = t;
      const keep = Math.exp(-dt / FLING_DECAY_MS);
      this.scroll((vx * dt * WHEEL_NOTCH) / TOUCH_WHEEL_STEP_PX, (vy * dt * WHEEL_NOTCH) / TOUCH_WHEEL_STEP_PX);
      vx *= keep;
      vy *= keep;
      if (Math.hypot(vx, vy) >= FLING_STOP) this.fling = requestAnimationFrame(step);
    };
    this.fling = requestAnimationFrame(step);
  }

  /** Scrolls by wheel units (fractions add up); positive = down / right. */
  private scroll(dx: number, dy: number): void {
    this.wheelX += dx;
    this.wheelY += dy;
    const sx = Math.max(-32767, Math.min(32767, Math.trunc(this.wheelX)));
    const sy = Math.max(-32767, Math.min(32767, Math.trunc(this.wheelY)));
    if (sx === 0 && sy === 0) return;
    this.wheelX -= sx;
    this.wheelY -= sy;
    const m = new Uint8Array(6);
    const v = new DataView(m.buffer);
    m[0] = C_WHEEL;
    v.setInt16(2, sx, true);
    v.setInt16(4, sy, true);
    this.send(m);
  }

  private onKey(e: KeyboardEvent, down: boolean): void {
    // Ctrl+Alt+Shift+Z (Ctrl+Option+Shift+Z on a Mac) turns capture mode on
    // or off, also from a field of the menu; its Z never reaches the remote
    // side (the modifiers did, and are released there as usual).
    if (e.code === 'KeyZ' && e.ctrlKey && e.altKey && e.shiftKey) {
      e.preventDefault();
      if (down && !e.repeat) this.setCapture(!this.capture);
      return;
    }
    // Keys belong to the remote side only while the picture has the focus;
    // elsewhere (the menu's fields, the page after a click outside the
    // picture) they do what they do in any page, pasting included.
    if (e.target !== this.canvas) return;
    const code = EVDEV[e.code];
    if (code === undefined) return;
    e.preventDefault();
    // The V of a paste is the server's to press: its repeats and its
    // release stay here.
    if (code === EVDEV.KeyV && this.pasteV) {
      if (!down) this.pasteV = false;
      return;
    }
    if (this.paste) {
      this.queued.push([code, down]);
      return;
    }
    const ctrlFirst = this.keys.has(EVDEV.ControlLeft) || this.keys.has(EVDEV.ControlRight);
    // The clipboard can be read only on HTTPS or localhost pages.
    if (down && code === EVDEV.KeyV && ctrlFirst && !e.altKey && !e.metaKey && window.isSecureContext) {
      this.startPaste();
      return;
    }
    this.setKey(code, down);
  }

  /** Presses or releases a key (evdev code). A key stays down on the remote
   * side until released, so holding it repeats there: the browser's own
   * repeats of a held key are dropped here. */
  setKey(code: number, down: boolean): void {
    if (down) {
      if (this.keys.has(code)) return;
      this.keys.add(code);
    } else if (!this.keys.delete(code)) {
      return;
    }
    const m = new Uint8Array(4);
    m[0] = C_KEY;
    m[1] = down ? 1 : 0;
    new DataView(m.buffer).setUint16(2, code, true);
    this.send(m);
  }

  /** Types characters on the remote side, whatever its keyboard layout. */
  text(text: string): void {
    const body = new TextEncoder().encode(text);
    const m = new Uint8Array(1 + body.length);
    m[0] = C_TEXT;
    m.set(body, 1);
    this.send(m);
  }

  private releaseAll(): void {
    // A paste waiting for the clipboard (its permission prompt took the
    // focus) is dropped with what was held back.
    this.paste = null;
    this.queued = [];
    for (const k of [...this.keys]) this.setKey(k, false);
    for (const b of this.buttons) this.send(new Uint8Array([C_BUTTON, b, 0]));
    this.buttons.clear();
  }

  /** Ctrl (pressed first) + V: the V is held back, this computer's clipboard
   * goes to the remote one, and the server presses the V once it is there.
   * Keys meanwhile wait, so Ctrl stays down on the remote side until then. */
  private startPaste(): void {
    const paste = { done: 0 };
    this.paste = paste;
    this.pasteV = true;
    void navigator.clipboard.readText().then(
      (text) => {
        if (this.paste !== paste) return;
        this.sendPaste(text);
        // The server answers (`pasted`); if it never does, keys go on.
        paste.done = window.setTimeout(() => this.pasted(), PASTE_WAIT_MS);
      },
      () => {
        if (this.paste !== paste) return;
        // No permission (or no text): a plain Ctrl+V of the remote clipboard.
        this.setKey(EVDEV.KeyV, true);
        this.setKey(EVDEV.KeyV, false);
        this.pasted();
      },
    );
  }

  /** The server pressed the V (`S_PASTED`): held-back keys go out. */
  pasted(): void {
    if (!this.paste) return;
    clearTimeout(this.paste.done);
    this.paste = null;
    const queued = this.queued;
    this.queued = [];
    for (const [code, down] of queued) this.setKey(code, down);
  }
}
