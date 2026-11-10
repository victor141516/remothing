import { EVDEV } from './keymap';
import { C_BUTTON, C_KEY, C_MOVE, C_TEXT, C_WHEEL } from './proto';

// Browser MouseEvent.button -> X button.
const BUTTONS = [1, 2, 3, 8, 9];
const LEFT = 1;
const RIGHT = 3;
const WHEEL_STEP_PX = 100;
/** Two fingers dragged this far scroll one wheel step. */
const TOUCH_WHEEL_STEP_PX = 24;
/** A finger never rests perfectly still: fingers that travel less than this, all together, made a tap. */
const TAP_SLOP_PX = 10;

export class Input {
  private keys = new Set<number>();
  private buttons = new Set<number>();
  /** Wheel steps not sent yet (less than one). */
  private wheelX = 0;
  private wheelY = 0;
  /** Fingers on the screen, with their last position. */
  private touches = new Map<number, { x: number; y: number }>();
  /** The touch gesture going on: most fingers down at once, and how far they travelled. */
  private fingers = 0;
  private travel = 0;
  /** Remote screen size; the canvas itself belongs to the render worker. */
  screenWidth = 0;
  screenHeight = 0;
  /** Where this client last put the remote pointer (remote pixels). */
  pointerX = 0;
  pointerY = 0;
  /** The pointer is driven by touch: the browser shows no cursor for it. */
  touch = false;

  /** `pointerChanged`: `touch` changed, or the pointer moved by touch. */
  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly send: (msg: Uint8Array) => void,
    private readonly pointerChanged: () => void,
  ) {
    // pointerrawupdate fires as soon as the OS delivers motion, not aligned to frames.
    const moveEvent = 'onpointerrawupdate' in window ? 'pointerrawupdate' : 'pointermove';
    // A mouse or pen points at a place of the picture.
    canvas.addEventListener(moveEvent, (e) => this.onMove(e as PointerEvent));
    canvas.addEventListener('pointerdown', (e) => {
      if (e.pointerType === 'touch') return;
      canvas.focus();
      canvas.setPointerCapture(e.pointerId);
      this.onMove(e);
      this.button(e.button, true);
      e.preventDefault();
    });
    canvas.addEventListener('pointerup', (e) => {
      if (e.pointerType === 'touch') return;
      this.onMove(e);
      this.button(e.button, false);
      e.preventDefault();
    });
    // Touch works as on a trackpad, and so anywhere around the picture too
    // (a wide screen on an upright phone is a narrow strip): one finger
    // moves the pointer from where it is, a tap clicks, a tap with two
    // fingers is the right button, two fingers dragged scroll.
    const body = document.body;
    body.addEventListener('pointerdown', (e) => {
      if (e.pointerType !== 'touch' || (e.target !== canvas && e.target !== body)) return;
      // No focus change (a phone's keyboard would close) and no mouse events made up from the touch.
      e.preventDefault();
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
    const r = this.canvas.getBoundingClientRect();
    const x = Math.round(((e.clientX - r.left) * this.screenWidth) / r.width);
    const y = Math.round(((e.clientY - r.top) * this.screenHeight) / r.height);
    this.pointerX = Math.max(0, Math.min(this.screenWidth - 1, x));
    this.pointerY = Math.max(0, Math.min(this.screenHeight - 1, y));
    this.moveTo(this.pointerX, this.pointerY);
    if (this.touch) {
      this.touch = false;
      this.pointerChanged();
    }
  }

  private button(browserButton: number, down: boolean): void {
    const b = BUTTONS[browserButton];
    if (b === undefined) return;
    if (down) this.buttons.add(b);
    else if (!this.buttons.delete(b)) return;
    this.send(new Uint8Array([C_BUTTON, b, down ? 1 : 0]));
  }

  private touchDown(e: PointerEvent): void {
    if (this.touches.size === 0) {
      this.fingers = 0;
      this.travel = 0;
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
    if (this.fingers === 1) {
      // As far on the remote screen as the finger went over its picture.
      const scale = this.screenWidth / this.canvas.getBoundingClientRect().width;
      this.pointerX = Math.max(0, Math.min(this.screenWidth - 1, this.pointerX + dx * scale));
      this.pointerY = Math.max(0, Math.min(this.screenHeight - 1, this.pointerY + dy * scale));
      this.moveTo(Math.round(this.pointerX), Math.round(this.pointerY));
      this.pointerChanged();
    } else if (this.touches.size === 2) {
      // Each finger reports its half of the drag. What is on the screen
      // follows the fingers, as when scrolling the phone's own pages.
      this.scroll(-dx / 2 / TOUCH_WHEEL_STEP_PX, -dy / 2 / TOUCH_WHEEL_STEP_PX);
    }
  }

  /** A finger left the screen, or (`completed` false) the browser took the touch away. */
  private touchUp(e: PointerEvent, completed: boolean): void {
    if (!this.touches.delete(e.pointerId)) return;
    if (!completed) this.travel = Infinity;
    if (this.touches.size > 0 || this.travel >= TAP_SLOP_PX || this.screenWidth === 0) return;
    const button = this.fingers === 1 ? LEFT : this.fingers === 2 ? RIGHT : undefined;
    if (button === undefined) return;
    // The click goes where the pointer is drawn, even if something else moved the remote one since.
    this.moveTo(Math.round(this.pointerX), Math.round(this.pointerY));
    this.send(new Uint8Array([C_BUTTON, button, 1]));
    this.send(new Uint8Array([C_BUTTON, button, 0]));
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    const scale = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 33 : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? 400 : 1;
    this.scroll((e.deltaX * scale) / WHEEL_STEP_PX, (e.deltaY * scale) / WHEEL_STEP_PX);
  }

  /** Scrolls by wheel steps (fractions add up); positive = down / right. */
  private scroll(dx: number, dy: number): void {
    this.wheelX += dx;
    this.wheelY += dy;
    const sx = Math.trunc(this.wheelX);
    const sy = Math.trunc(this.wheelY);
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
    // Text typed into the control bar's fields is theirs.
    const t = e.target;
    if (t instanceof HTMLTextAreaElement || t instanceof HTMLSelectElement || t instanceof HTMLInputElement) return;
    const code = EVDEV[e.code];
    if (code === undefined) return;
    e.preventDefault();
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
    for (const k of [...this.keys]) this.setKey(k, false);
    for (const b of this.buttons) this.send(new Uint8Array([C_BUTTON, b, 0]));
    this.buttons.clear();
  }
}
