import { EVDEV } from './keymap';
import { C_BUTTON, C_KEY, C_MOVE, C_WHEEL } from './proto';

// Browser MouseEvent.button -> X button.
const BUTTONS = [1, 2, 3, 8, 9];
const WHEEL_STEP_PX = 100;

export class Input {
  private keys = new Set<number>();
  private buttons = new Set<number>();
  private wheelX = 0;
  private wheelY = 0;
  /** Remote screen size; the canvas itself belongs to the render worker. */
  screenWidth = 0;
  screenHeight = 0;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly send: (msg: Uint8Array) => void,
  ) {
    // pointerrawupdate fires as soon as the OS delivers motion, not aligned to frames.
    const moveEvent = 'onpointerrawupdate' in window ? 'pointerrawupdate' : 'pointermove';
    canvas.addEventListener(moveEvent, (e) => this.onMove(e as PointerEvent));
    canvas.addEventListener('pointerdown', (e) => {
      canvas.focus();
      canvas.setPointerCapture(e.pointerId);
      this.onMove(e);
      this.button(e.button, true);
      e.preventDefault();
    });
    canvas.addEventListener('pointerup', (e) => {
      this.onMove(e);
      this.button(e.button, false);
      e.preventDefault();
    });
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());
    canvas.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    window.addEventListener('keydown', (e) => this.onKey(e, true));
    window.addEventListener('keyup', (e) => this.onKey(e, false));
    window.addEventListener('blur', () => this.releaseAll());
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
    if (this.screenWidth === 0) return;
    const r = this.canvas.getBoundingClientRect();
    const x = Math.round(((e.clientX - r.left) * this.screenWidth) / r.width);
    const y = Math.round(((e.clientY - r.top) * this.screenHeight) / r.height);
    this.moveTo(Math.max(0, Math.min(this.screenWidth - 1, x)), Math.max(0, Math.min(this.screenHeight - 1, y)));
  }

  private button(browserButton: number, down: boolean): void {
    const b = BUTTONS[browserButton];
    if (b === undefined) return;
    if (down) this.buttons.add(b);
    else if (!this.buttons.delete(b)) return;
    this.send(new Uint8Array([C_BUTTON, b, down ? 1 : 0]));
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    const scale = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 33 : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? 400 : 1;
    this.wheelX += e.deltaX * scale;
    this.wheelY += e.deltaY * scale;
    const sx = Math.trunc(this.wheelX / WHEEL_STEP_PX);
    const sy = Math.trunc(this.wheelY / WHEEL_STEP_PX);
    if (sx === 0 && sy === 0) return;
    this.wheelX -= sx * WHEEL_STEP_PX;
    this.wheelY -= sy * WHEEL_STEP_PX;
    const m = new Uint8Array(6);
    const v = new DataView(m.buffer);
    m[0] = C_WHEEL;
    v.setInt16(2, sx, true);
    v.setInt16(4, sy, true);
    this.send(m);
  }

  private onKey(e: KeyboardEvent, down: boolean): void {
    const code = EVDEV[e.code];
    if (code === undefined) return;
    e.preventDefault();
    // The X server autorepeats held keys itself.
    if (down && e.repeat) return;
    if (down) this.keys.add(code);
    else if (!this.keys.delete(code)) return;
    this.key(code, down);
  }

  private key(code: number, down: boolean): void {
    const m = new Uint8Array(4);
    m[0] = C_KEY;
    m[1] = down ? 1 : 0;
    new DataView(m.buffer).setUint16(2, code, true);
    this.send(m);
  }

  private releaseAll(): void {
    for (const k of this.keys) this.key(k, false);
    this.keys.clear();
    for (const b of this.buttons) this.send(new Uint8Array([C_BUTTON, b, 0]));
    this.buttons.clear();
  }
}
