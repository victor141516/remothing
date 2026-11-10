// On-screen keyboard, for devices without one. Keys are physical positions
// (the evdev codes a real keyboard sends through input.ts): the remote
// side's layout decides the character, and the labels only show what a
// layout prints on them. A key stays down while it is touched, so holding
// it repeats on the remote side; modifiers latch until the next key.

import { EVDEV } from './keymap';

type Layout = 'us' | 'es';
const LAYOUTS: Layout[] = ['us', 'es'];
const LAYOUT_KEY = 'remothing.keyboard';

/** [plain, shifted] of the keys whose character depends on the layout. */
const SYMBOLS: Record<Layout, Record<string, [string, string]>> = {
  us: {
    Backquote: ['`', '~'], Digit1: ['1', '!'], Digit2: ['2', '@'], Digit3: ['3', '#'], Digit4: ['4', '$'],
    Digit5: ['5', '%'], Digit6: ['6', '^'], Digit7: ['7', '&'], Digit8: ['8', '*'], Digit9: ['9', '('],
    Digit0: ['0', ')'], Minus: ['-', '_'], Equal: ['=', '+'], BracketLeft: ['[', '{'], BracketRight: [']', '}'],
    Backslash: ['\\', '|'], Semicolon: [';', ':'], Quote: ["'", '"'], IntlBackslash: ['\\', '|'],
    Comma: [',', '<'], Period: ['.', '>'], Slash: ['/', '?'],
  },
  es: {
    Backquote: ['º', 'ª'], Digit1: ['1', '!'], Digit2: ['2', '"'], Digit3: ['3', '·'], Digit4: ['4', '$'],
    Digit5: ['5', '%'], Digit6: ['6', '&'], Digit7: ['7', '/'], Digit8: ['8', '('], Digit9: ['9', ')'],
    Digit0: ['0', '='], Minus: ["'", '?'], Equal: ['¡', '¿'], BracketLeft: ['`', '^'], BracketRight: ['+', '*'],
    Backslash: ['ç', 'Ç'], Semicolon: ['ñ', 'Ñ'], Quote: ['´', '¨'], IntlBackslash: ['<', '>'],
    Comma: [',', ';'], Period: ['.', ':'], Slash: ['-', '_'],
  },
};

/** Keys with the same label everywhere: [label, width in key units]. */
const NAMED: Record<string, [string, number]> = {
  Escape: ['Esc', 1.25], Backspace: ['⌫', 1.75], Tab: ['Tab', 1.5], Enter: ['⏎', 2], CapsLock: ['Caps', 1.75],
  ShiftLeft: ['⇧', 1.25], ShiftRight: ['⇧', 1.25], ControlLeft: ['Ctrl', 1.5], MetaLeft: ['Win', 1.25],
  AltLeft: ['Alt', 1.25], Space: ['', 5.5], AltRight: ['AltGr', 1.25], ArrowLeft: ['←', 1], ArrowRight: ['→', 1],
  ArrowUp: ['↑', 1], ArrowDown: ['↓', 1], Delete: ['Del', 1.25], Home: ['Home', 1.25], End: ['End', 1.25],
  PageUp: ['PgUp', 1.25], PageDown: ['PgDn', 1.25],
};

const MODIFIERS: Record<string, true> = {
  ShiftLeft: true, ShiftRight: true, ControlLeft: true, AltLeft: true, AltRight: true, MetaLeft: true,
};

const ROWS: string[][] = [
  ['Escape', 'F1', 'F2', 'F3', 'F4', 'F5', 'F6', 'F7', 'F8', 'F9', 'F10', 'F11', 'F12', 'Delete'],
  ['Backquote', 'Digit1', 'Digit2', 'Digit3', 'Digit4', 'Digit5', 'Digit6', 'Digit7', 'Digit8', 'Digit9', 'Digit0', 'Minus', 'Equal', 'Backspace'],
  ['Tab', 'KeyQ', 'KeyW', 'KeyE', 'KeyR', 'KeyT', 'KeyY', 'KeyU', 'KeyI', 'KeyO', 'KeyP', 'BracketLeft', 'BracketRight', 'Backslash'],
  ['CapsLock', 'KeyA', 'KeyS', 'KeyD', 'KeyF', 'KeyG', 'KeyH', 'KeyJ', 'KeyK', 'KeyL', 'Semicolon', 'Quote', 'Enter'],
  ['ShiftLeft', 'IntlBackslash', 'KeyZ', 'KeyX', 'KeyC', 'KeyV', 'KeyB', 'KeyN', 'KeyM', 'Comma', 'Period', 'Slash', 'ArrowUp', 'ShiftRight'],
  ['ControlLeft', 'MetaLeft', 'AltLeft', 'Space', 'AltRight', 'Home', 'End', 'ArrowLeft', 'ArrowDown', 'ArrowRight'],
];

export class OnScreenKeyboard {
  readonly element = document.createElement('div');
  private layout: Layout;
  private readonly keys = new Map<string, HTMLButtonElement>();
  /** Modifiers held down by a tap, released after the next other key. */
  private readonly latched = new Set<string>();

  constructor(private readonly send: (evdev: number, down: boolean) => void) {
    const saved = localStorage.getItem(LAYOUT_KEY);
    const guess: Layout = navigator.language.startsWith('es') ? 'es' : 'us';
    this.layout = LAYOUTS.find((l) => l === saved) ?? guess;
    this.element.id = 'ui-keyboard';
    this.element.hidden = true;
    for (const codes of ROWS) {
      const row = document.createElement('div');
      row.className = 'kb-row';
      for (const code of codes) row.append(this.key(code));
      this.element.append(row);
    }
    // Which layout's characters the labels show (the remote side has its own).
    const switcher = document.createElement('button');
    switcher.className = 'kb-key kb-layout';
    switcher.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      this.layout = LAYOUTS[(LAYOUTS.indexOf(this.layout) + 1) % LAYOUTS.length];
      localStorage.setItem(LAYOUT_KEY, this.layout);
      this.relabel();
    });
    this.keys.set('layout', switcher);
    this.element.lastElementChild!.append(switcher);
    this.relabel();
  }

  private key(code: string): HTMLButtonElement {
    const b = document.createElement('button');
    b.className = 'kb-key';
    b.style.flexGrow = String(NAMED[code]?.[1] ?? 1);
    const evdev = EVDEV[code];
    if (MODIFIERS[code]) {
      b.addEventListener('pointerdown', (e) => {
        e.preventDefault();
        this.latch(code, !this.latched.has(code));
      });
    } else {
      b.addEventListener('pointerdown', (e) => {
        // No focus change (typing must keep going to the remote screen)
        // and no synthetic mouse events.
        e.preventDefault();
        b.setPointerCapture(e.pointerId);
        b.classList.add('down');
        this.send(evdev, true);
      });
      const release = () => {
        if (!b.classList.contains('down')) return;
        b.classList.remove('down');
        this.send(evdev, false);
        for (const m of [...this.latched]) this.latch(m, false);
      };
      b.addEventListener('pointerup', release);
      b.addEventListener('pointercancel', release);
    }
    this.keys.set(code, b);
    return b;
  }

  private latch(code: string, down: boolean): void {
    if (down) this.latched.add(code);
    else this.latched.delete(code);
    this.keys.get(code)!.classList.toggle('down', down);
    this.send(EVDEV[code], down);
    this.relabel();
  }

  /** Releases what is latched (the keyboard is being hidden). */
  release(): void {
    for (const m of [...this.latched]) this.latch(m, false);
  }

  private relabel(): void {
    const shift = this.latched.has('ShiftLeft') || this.latched.has('ShiftRight') ? 1 : 0;
    for (const [code, b] of this.keys) {
      const symbol = SYMBOLS[this.layout][code];
      if (symbol) b.textContent = symbol[shift];
      else if (code.startsWith('Key')) b.textContent = shift ? code[3] : code[3].toLowerCase();
      else if (code === 'layout') b.textContent = this.layout.toUpperCase();
      else b.textContent = NAMED[code]?.[0] ?? code;
    }
  }
}
