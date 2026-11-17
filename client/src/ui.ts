// The controls menu over the stream. A small tab at the top centre pulls one
// panel down from the top edge; the panel shows one view at a time: the
// root (three groups of tiles, each saying its state in words), the Displays
// or Clipboard page (drill-down with a back button), a yes/no question or
// the message that the stream stopped. Its height follows the view, so it
// grows into a function and shrinks back. A choice whose effect shows on the
// page closes it; so do Esc, the tab and a click or tap outside, which is
// then not sent to the remote computer. While the mouse is captured the tab
// hides; while capture mode waits for a click on the picture, the tab says so.

import { DisplaysPage } from './displays';
import { OnScreenKeyboard } from './keyboard';
import { TextEntry } from './textentry';

declare global {
  interface Navigator {
    /** Keyboard Lock API (Chromium, secure contexts). */
    keyboard?: { lock(keys?: string[]): Promise<void>; unlock(): void };
  }
}

/** Size, refresh rate and HDR of a virtual display (proto.rs `DisplayMode`). */
export interface DisplayMode {
  width: number;
  height: number;
  refresh_hz: number;
  hdr: boolean;
}

export interface DisplayEntry {
  id: string;
  name: string;
  width: number;
  height: number;
  kind: 'real' | 'virtual';
  /** The mode of a virtual display a client may change or remove. */
  mode?: DisplayMode;
}

interface VirtualCapabilities {
  max_width: number;
  max_height: number;
  refresh_rates: number[];
  hdr: boolean;
  resize: boolean;
}

interface VirtualSetup {
  state: 'ready' | 'install' | 'unavailable';
  notice?: string;
  reason?: string;
}

/** The display list (S_DISPLAYS). */
export interface DisplaysMessage {
  displays: DisplayEntry[];
  current: string | null;
  virtual: { setup: VirtualSetup; capabilities: VirtualCapabilities } | null;
  /** Why this client's last virtual display request failed. */
  error?: string;
}

/** A virtual display request (C_VIRTUAL). */
export type VirtualRequest =
  | { op: 'install' }
  | { op: 'create'; mode: DisplayMode }
  | { op: 'set'; id: string; mode: DisplayMode }
  | { op: 'destroy'; id: string };

export interface UiActions {
  key(evdev: number, down: boolean): void;
  /** Types characters on the remote side. */
  text(text: string): void;
  /** Puts text on the remote clipboard. */
  clipboard(text: string): void;
  /** Asks for the display list (answered through `Ui.displays`). */
  listDisplays(): void;
  selectDisplay(id: string): void;
  virtualDisplay(request: VirtualRequest): void;
  /** The area the picture can take, in device pixels, and this screen's refresh rate (0: unknown). */
  viewport(): { width: number; height: number; hz: number };
  /** Shows or hides the stats overlay; whether it shows now. */
  toggleStats(): boolean;
  /** Switches the remote session's sound on or off. */
  toggleSound(): void;
  /** Switches this device's microphone into the remote session, or out of it. */
  toggleMic(): void;
  /** Turns mouse capture (games) on or off. */
  toggleCapture(): void;
  /** The space left for the picture changed (the keyboard opened or closed). */
  layout(): void;
}

/** Typing pauses this long before the text goes to the remote clipboard. */
const CLIPBOARD_SEND_MS = 300;
/** How long the "Mouse captured" note stays. */
const FLASH_MS = 2500;
const MOD = navigator.platform.startsWith('Mac') ? 'Ctrl+Option+Shift+Z' : 'Ctrl+Alt+Shift+Z';

type View = 'closed' | 'root' | 'displays' | 'clipboard' | 'ask' | 'stopped';
type Page = 'root' | 'displays' | 'clipboard';
type Look = 'on' | 'off' | 'armed' | 'pending' | 'unavailable';

/** A root tile: label on line 1, state word and detail on line 2. */
interface Tile {
  readonly element: HTMLButtonElement;
  set(look: Look | null, word: string, detail?: string, disabled?: boolean): void;
}

export class Ui {
  private readonly root: HTMLElement;
  private readonly handle: HTMLButtonElement;
  private readonly flash = document.createElement('div');
  private readonly clipboardBox = document.createElement('textarea');
  private readonly keyboard: OnScreenKeyboard;
  private readonly textEntry: TextEntry;
  private readonly page: DisplaysPage;
  private readonly askText = document.createElement('p');
  private readonly stoppedText = document.createElement('p');
  private readonly tiles: Record<'fullscreen' | 'stats' | 'display' | 'capture' | 'type' | 'osk' | 'clip' | 'sound' | 'mic', Tile>;

  private open = false;
  private current: Page = 'root';
  private question = false;
  private ended = false;
  /** Whether the question opened the menu (it closes again after the answer). */
  private openedByQuestion = false;
  /** Answers the question on screen, if there is one. */
  private answer: ((yes: boolean) => void) | null = null;
  private clipboardTimer = 0;
  private flashTimer = 0;
  private locked = false;
  private keyboardLocked = false;
  /** The gesture that closed the menu: none of it is remote input. */
  private swallowed: number | null = null;

  constructor(root: HTMLElement, private readonly actions: UiActions) {
    this.root = root;
    this.handle = this.button('', 'Controls', () => {
      if (this.open) this.close();
      else this.openMenu();
    });
    this.handle.id = 'ui-handle';
    this.handle.append(span('grip', '▾'), span('hint', 'Click the picture to capture the mouse'));
    this.flash.id = 'ui-flash';
    this.flash.setAttribute('role', 'status');

    const canTouch = navigator.maxTouchPoints > 0;
    const t = (id: string, label: string, onClick: () => void, drill = false) => this.tile(id, label, onClick, drill);
    this.tiles = {
      fullscreen: t('ui-fullscreen', 'Fullscreen', () => this.after(() => void this.toggleFullscreen())),
      stats: t('ui-stats', 'Stats', () => this.after(() => this.showStats(this.actions.toggleStats()))),
      display: t('ui-display', 'Display', () => this.go('displays'), true),
      capture: t('ui-capture', 'Capture mouse', () => this.after(() => this.actions.toggleCapture())),
      type: t('ui-type', 'Type', () => this.after(() => this.textEntry.toggle())),
      osk: t('ui-osk', 'On-screen keyboard', () => this.after(() => this.toggleKeyboard())),
      clip: t('ui-clip', 'Clipboard', () => this.go('clipboard'), true),
      sound: t('ui-sound', 'Sound', () => this.actions.toggleSound()),
      mic: t('ui-mic', 'Microphone', () => this.actions.toggleMic()),
    };
    // Only with a mouse or trackpad: a touch screen has no pointer to trap.
    this.tiles.capture.element.hidden = !matchMedia('(any-pointer: fine)').matches;
    // Only with a touch screen: a computer has its keyboard already.
    this.tiles.type.element.hidden = !canTouch;
    // Browsers only give the microphone to secure pages.
    this.tiles.mic.element.hidden = navigator.mediaDevices?.getUserMedia === undefined;

    const group = (caption: string, ...tiles: Tile[]) => {
      const g = document.createElement('div');
      g.className = 'group';
      const row = document.createElement('div');
      row.className = 'tiles';
      row.append(...tiles.map((x) => x.element));
      g.append(heading('h3', caption), row);
      return g;
    };
    const rootView = section('ui-root');
    rootView.append(
      group('Screen', this.tiles.fullscreen, this.tiles.stats, this.tiles.display),
      group('Control', this.tiles.type, this.tiles.capture, this.tiles.osk, this.tiles.clip),
      group('Sound', this.tiles.sound, this.tiles.mic),
    );

    this.page = new DisplaysPage(
      actions,
      (q) => this.ask(q),
      (label, title, onClick) => this.button(label, title, onClick),
      () => this.showDisplayTile(),
      () => this.toPicture(),
    );
    const displaysView = section('ui-displays');
    displaysView.append(this.pageHead('Displays', this.page.status), this.page.body);

    this.clipboardBox.id = 'ui-clipboard-box';
    this.clipboardBox.placeholder = 'Nothing copied on the remote computer yet.';
    this.clipboardBox.spellcheck = false;
    this.clipboardBox.addEventListener('input', () => {
      clearTimeout(this.clipboardTimer);
      this.clipboardTimer = setTimeout(() => this.flushClipboard(), CLIPBOARD_SEND_MS);
      this.showClipTile();
    });
    const clipboardView = section('ui-clipboard');
    clipboardView.append(
      this.pageHead('Clipboard'),
      this.clipboardBox,
      paragraph('note', 'What is copied on the remote computer appears here. What you type or paste here is copied there.'),
    );

    const askView = section('ui-ask');
    this.askText.id = 'ui-ask-text';
    const no = this.button('No', '', () => this.answer?.(false));
    const yes = this.button('Yes', '', () => this.answer?.(true));
    yes.classList.add('primary');
    const answers = document.createElement('div');
    answers.className = 'actions';
    answers.append(no, yes);
    askView.append(this.askText, answers);

    const stoppedView = section('ui-stopped');
    stoppedView.setAttribute('role', 'alert');
    this.stoppedText.id = 'ui-stopped-text';
    stoppedView.append(heading('h2', 'The stream stopped'), this.stoppedText);

    const menu = document.createElement('div');
    menu.id = 'ui-menu';
    menu.append(rootView, displaysView, clipboardView, askView, stoppedView);

    this.keyboard = new OnScreenKeyboard(actions.key);
    this.textEntry = new TextEntry(actions, (on) => this.showTypeTile(on));
    root.append(this.handle, this.flash, menu, this.textEntry.element, this.keyboard.element);

    this.showFullscreenTile();
    this.showStats(false);
    this.showDisplayTile();
    this.capture(false, false);
    this.showTypeTile(false);
    this.showKeyboardTile();
    this.showClipTile();
    this.show();

    document.addEventListener('fullscreenchange', () => {
      if (document.fullscreenElement === null) {
        navigator.keyboard?.unlock();
        this.keyboardLocked = false;
      }
      this.showFullscreenTile();
    });
    // Before input.ts sees them (capture phase on window).
    window.addEventListener('pointerdown', (e) => this.outside(e), true);
    for (const type of ['pointermove', 'pointerrawupdate', 'pointerup', 'pointercancel'] as const) {
      window.addEventListener(type, (e) => this.swallow(e as PointerEvent), true);
    }
    window.addEventListener('keydown', (e) => this.escape(e), true);
    const room = () => this.room();
    window.addEventListener('resize', room);
    window.visualViewport?.addEventListener('resize', room);
    window.visualViewport?.addEventListener('scroll', room);
    this.room();
  }

  /** Height the on-screen keyboard takes at the bottom of the window. */
  get reservedBottom(): number {
    return this.keyboard.element.offsetHeight;
  }

  /** The remote clipboard changed. */
  remoteClipboard(text: string): void {
    // Text being typed here is newer than what comes back.
    if (this.clipboardTimer === 0 && this.clipboardBox.value !== text) this.clipboardBox.value = text;
    this.showClipTile();
  }

  /** Whether the remote session's sound plays, and the reason when it does not. */
  sound(playing: boolean, status: string): void {
    const tile = this.tiles.sound;
    if (playing) tile.set('on', 'On', status.replace(/^on: /, ''));
    else if (/^(unavailable|stopped)/.test(status)) tile.set('unavailable', 'Unavailable', status.replace(/^unavailable: /, ''));
    else if (status === 'off' || status.startsWith('off:')) tile.set('off', 'Off', status.replace(/^off:? ?/, ''));
    else tile.set('pending', 'Waiting', status);
  }

  /** Mouse capture: the mode, and whether the mouse is trapped right now. */
  capture(mode: boolean, locked: boolean): void {
    const tile = this.tiles.capture;
    if (locked) tile.set('on', 'On', `${MOD} frees the mouse`);
    else if (mode) tile.set('armed', 'Click the picture', `Esc or ${MOD} leaves`);
    else tile.set('off', 'Off', MOD);
    this.root.dataset.capture = locked ? 'locked' : mode ? 'armed' : 'off';
    if (locked && !this.locked) {
      // Nothing of the menu can be reached now, and the tab must not cover a game.
      if (this.open) this.close();
      this.flash.textContent = `Mouse captured · ${MOD} frees it`;
      this.flash.classList.add('show');
      clearTimeout(this.flashTimer);
      this.flashTimer = setTimeout(() => this.flash.classList.remove('show'), FLASH_MS);
    }
    this.locked = locked;
  }

  /** This device's microphone in the remote session (`Microphone.state`), and why. */
  microphone(state: 'off' | 'pending' | 'on' | 'taken', status: string): void {
    const tile = this.tiles.mic;
    if (state === 'on') tile.set('on', 'On', status.replace(/^on: /, ''));
    else if (state === 'pending') tile.set('pending', 'Waiting', status);
    // Another client talks: nothing to switch.
    else if (state === 'taken') tile.set('unavailable', 'In use', status, true);
    else if (/^(unavailable|stopped)/.test(status)) tile.set('unavailable', 'Unavailable', status.replace(/^unavailable: /, ''));
    else tile.set('off', 'Off', status.replace(/^off:? ?/, ''));
  }

  /** Asks a yes/no question in the menu; a newer question answers the old one with no. */
  ask(question: string): Promise<boolean> {
    this.answer?.(false);
    if (this.ended) return Promise.resolve(false);
    this.askText.textContent = question;
    this.openedByQuestion = !this.open;
    this.question = true;
    this.toPicture();
    this.show();
    const { promise, resolve } = Promise.withResolvers<boolean>();
    this.answer = (yes) => {
      this.answer = null;
      this.question = false;
      if (this.openedByQuestion) this.open = false;
      this.show();
      resolve(yes);
    };
    return promise;
  }

  /** Shows why the stream stopped for good; nothing else stays. */
  stopped(text: string): void {
    this.answer?.(false);
    this.ended = true;
    this.stoppedText.textContent = text;
    this.keyboard.release();
    this.keyboard.element.hidden = true;
    this.textEntry.element.blur();
    this.toPicture();
    this.show();
  }

  displays(message: DisplaysMessage): void {
    this.page.update(message);
  }

  // ---- views

  private get view(): View {
    if (this.ended) return 'stopped';
    if (this.question) return 'ask';
    return this.open ? this.current : 'closed';
  }

  private show(): void {
    const view = this.view;
    this.root.dataset.view = view;
    const open = view !== 'closed';
    this.handle.querySelector('.grip')!.textContent = open ? '▴' : '▾';
    this.handle.setAttribute('aria-expanded', String(open));
  }

  private openMenu(): void {
    this.open = true;
    this.current = 'root';
    // Other windows may have changed the displays.
    this.actions.listDisplays();
    this.show();
  }

  /** Closes the menu: typed clipboard text goes now, the page folds, keys go back to the picture. */
  private close(): void {
    this.flushClipboard();
    this.page.fold();
    this.open = false;
    this.current = 'root';
    this.toPicture();
    this.show();
  }

  private go(page: Page): void {
    if (page === 'root') {
      this.flushClipboard();
      this.page.fold();
      this.toPicture();
    }
    this.current = page;
    if (page === 'displays') this.actions.listDisplays();
    this.show();
    // A computer's clipboard box is ready for Ctrl+V / Ctrl+C; a phone's
    // keyboard is not raised just to read it.
    if (page === 'clipboard' && matchMedia('(hover: hover) and (pointer: fine)').matches) {
      this.clipboardBox.focus({ preventScroll: true });
      this.clipboardBox.select();
    }
  }

  /** A tile whose effect shows on the page: the menu closes first. */
  private after(action: () => void): void {
    this.close();
    action();
  }

  /** Focus out of the menu, back to the picture. A focused Type field keeps
   * the phone's keyboard, so focus elsewhere is left alone. */
  private toPicture(): void {
    const menu = document.getElementById('ui-menu');
    if (!menu?.contains(document.activeElement)) return;
    (document.activeElement as HTMLElement).blur();
    document.getElementById('screen')?.focus({ preventScroll: true });
  }

  private pageHead(title: string, status?: HTMLElement): HTMLElement {
    const head = document.createElement('header');
    head.className = 'head';
    const back = this.button('‹', 'Back', () => this.go('root'));
    back.className = 'back';
    back.setAttribute('aria-label', 'Back');
    head.append(back, heading('h2', title));
    if (status) head.append(status);
    return head;
  }

  // ---- outside clicks, Esc, room

  /** A click or tap outside an open menu closes it, and is not remote input. */
  private outside(e: PointerEvent): void {
    if (this.view === 'closed' || this.view === 'ask' || this.view === 'stopped') return;
    const target = e.target as Node;
    const menu = document.getElementById('ui-menu');
    if (menu?.contains(target) || this.handle.contains(target)) return;
    this.close();
    // A key of the on-screen keyboard works as usual.
    if (this.keyboard.element.contains(target)) return;
    this.swallowed = e.pointerId;
    e.preventDefault();
    e.stopImmediatePropagation();
  }

  private swallow(e: PointerEvent): void {
    if (e.pointerId !== this.swallowed) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    if (e.type === 'pointerup' || e.type === 'pointercancel') this.swallowed = null;
  }

  /** Esc closes an open menu (and is not sent); closed, it goes to the remote computer. */
  private escape(e: KeyboardEvent): void {
    if (e.key !== 'Escape' || e.repeat) return;
    const view = this.view;
    if (view !== 'root' && view !== 'displays' && view !== 'clipboard') return;
    e.preventDefault();
    e.stopImmediatePropagation();
    this.close();
  }

  /** The panel stays above the docked keyboard and within the visible area. */
  private room(): void {
    const view = window.visualViewport;
    const free = (view?.height ?? window.innerHeight) - this.reservedBottom;
    this.root.style.setProperty('--ui-free', `${Math.max(0, free)}px`);
    this.root.style.setProperty('--ui-top', `${view?.offsetTop ?? 0}px`);
  }

  // ---- tiles

  private tile(id: string, label: string, onClick: () => void, drill: boolean): Tile {
    const element = this.button('', '', onClick);
    element.id = id;
    element.className = drill ? 'tile drill' : 'tile';
    const state = span('state', '');
    element.append(span('label', label), ...(drill ? [span('chev', '›')] : []), state);
    const set = (look: Look | null, word: string, detail = '', disabled = false) => {
      element.className = ['tile', drill ? 'drill' : '', look ?? ''].filter(Boolean).join(' ');
      if (look === 'on' || look === 'off' || look === 'armed') element.setAttribute('aria-pressed', String(look !== 'off'));
      element.disabled = disabled;
      const parts: (Node | string)[] = [];
      if (word) {
        const b = document.createElement('b');
        b.textContent = word;
        parts.push(b);
      }
      if (detail) parts.push(word ? ` · ${detail}` : detail);
      state.replaceChildren(...parts);
    };
    return { element, set };
  }

  private showFullscreenTile(): void {
    const tile = this.tiles.fullscreen;
    if (!document.fullscreenEnabled) tile.set('unavailable', 'Unavailable', 'this browser has no fullscreen', true);
    else if (document.fullscreenElement) tile.set('on', 'On', this.keyboardLocked ? 'hold Esc to leave' : 'Esc leaves');
    else tile.set('off', 'Off', navigator.keyboard ? 'locks system keys' : '');
  }

  private showStats(on: boolean): void {
    this.tiles.stats.set(on ? 'on' : 'off', on ? 'On' : 'Off', on ? '' : 'latency numbers');
  }

  /** The stats overlay shows (or not) from the start (`?stats`). */
  statsShown(on: boolean): void {
    this.showStats(on);
  }

  private showDisplayTile(): void {
    const d = this.page.current;
    const { text, error } = this.page.line;
    const tile = this.tiles.display;
    if (text) {
      tile.set(null, '', text);
      tile.element.classList.toggle('error', error);
    } else {
      tile.set(null, '', d ? `${d.name || d.id} · ${d.width}×${d.height}` : '…');
    }
  }

  private showTypeTile(on: boolean): void {
    this.tiles.type.set(on ? 'on' : 'off', on ? 'On' : 'Off', on ? 'typing goes to the remote' : "this device's keyboard");
  }

  private showKeyboardTile(): void {
    const on = !this.keyboard.element.hidden;
    const typeShown = !this.tiles.type.element.hidden;
    this.tiles.osk.set(on ? 'on' : 'off', on ? 'On' : 'Off', on || !typeShown ? '' : 'full PC keyboard');
  }

  private showClipTile(): void {
    // Never the text itself: it may be a password.
    const n = this.clipboardBox.value.length;
    this.tiles.clip.set(null, '', n ? `${n} characters` : 'Empty');
  }

  // ---- actions

  private toggleKeyboard(): void {
    this.keyboard.element.hidden = !this.keyboard.element.hidden;
    if (this.keyboard.element.hidden) this.keyboard.release();
    this.showKeyboardTile();
    this.actions.layout();
    this.room();
  }

  private flushClipboard(): void {
    if (this.clipboardTimer === 0) return;
    clearTimeout(this.clipboardTimer);
    this.clipboardTimer = 0;
    this.actions.clipboard(this.clipboardBox.value);
  }

  private button(label: string, title: string, onClick: () => void): HTMLButtonElement {
    const b = document.createElement('button');
    b.textContent = label;
    if (title) b.title = title;
    // Never focused: keys keep going to the remote screen.
    b.tabIndex = -1;
    b.addEventListener('mousedown', (e) => e.preventDefault());
    b.addEventListener('click', onClick);
    return b;
  }

  private async toggleFullscreen(): Promise<void> {
    if (document.fullscreenElement) {
      await document.exitFullscreen();
      return;
    }
    await document.documentElement.requestFullscreen();
    // With the lock, Alt+Tab, the Windows key or Esc reach the page (and so
    // the remote side) instead of the local system; holding Esc still
    // leaves fullscreen. Without the API (Firefox, Safari, plain HTTP)
    // fullscreen works as usual.
    this.keyboardLocked = (await navigator.keyboard?.lock().then(() => true, () => false)) ?? false;
    this.showFullscreenTile();
  }
}

function section(id: string): HTMLElement {
  const s = document.createElement('section');
  s.id = id;
  return s;
}

function heading(tag: 'h2' | 'h3', text: string): HTMLElement {
  const h = document.createElement(tag);
  h.textContent = text;
  return h;
}

function paragraph(className: string, text: string): HTMLElement {
  const p = document.createElement('p');
  p.className = className;
  p.textContent = text;
  return p;
}

function span(className: string, text: string): HTMLElement {
  const s = document.createElement('span');
  s.className = className;
  s.textContent = text;
  return s;
}
