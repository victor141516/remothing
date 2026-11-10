// Control bar over the stream: fullscreen with keyboard capture, which
// display is streamed, the remote clipboard as a text box, the on-screen
// keyboard, the phone's own keyboard and the stats overlay. A small handle
// at the top opens it.

import { OnScreenKeyboard } from './keyboard';
import { TextEntry } from './textentry';

declare global {
  interface Navigator {
    /** Keyboard Lock API (Chromium, secure contexts). */
    keyboard?: { lock(keys?: string[]): Promise<void>; unlock(): void };
  }
}

export interface DisplayEntry {
  id: string;
  name: string;
  width: number;
  height: number;
}

export interface UiActions {
  key(evdev: number, down: boolean): void;
  /** Types characters on the remote side. */
  text(text: string): void;
  /** Puts text on the remote clipboard. */
  clipboard(text: string): void;
  /** Asks for the display list (answered through `Ui.displays`). */
  listDisplays(): void;
  selectDisplay(id: string): void;
  toggleStats(): void;
  /** Switches the remote session's sound on or off. */
  toggleSound(): void;
  /** The space left for the picture changed (the keyboard opened or closed). */
  layout(): void;
}

/** Typing pauses this long before the text goes to the remote clipboard. */
const CLIPBOARD_SEND_MS = 300;

export class Ui {
  private readonly bar = document.createElement('div');
  private readonly displaySelect = document.createElement('select');
  private readonly clipboardBox = document.createElement('textarea');
  private readonly keyboard: OnScreenKeyboard;
  private readonly fullscreenButton: HTMLButtonElement;
  private readonly soundButton: HTMLButtonElement;
  private clipboardTimer = 0;

  constructor(root: HTMLElement, actions: UiActions) {
    const handle = this.button('☰', 'Controls', () => {
      this.bar.hidden = !this.bar.hidden;
      if (!this.bar.hidden) actions.listDisplays();
    });
    handle.id = 'ui-handle';

    this.bar.id = 'ui-bar';
    this.bar.hidden = true;
    this.fullscreenButton = this.button('Fullscreen', 'Fullscreen; captures system keys (Alt+Tab, Win, Esc) where the browser allows', () =>
      void this.toggleFullscreen(),
    );
    this.displaySelect.title = 'Display being streamed';
    this.displaySelect.addEventListener('change', () => {
      actions.selectDisplay(this.displaySelect.value);
      // Keys go back to the remote screen.
      this.displaySelect.blur();
    });
    const clipboardPanel = document.createElement('div');
    clipboardPanel.id = 'ui-clipboard';
    clipboardPanel.hidden = true;
    this.clipboardBox.placeholder = 'Remote clipboard: what is copied there appears here; what you type or paste here is copied there.';
    this.clipboardBox.spellcheck = false;
    this.clipboardBox.addEventListener('input', () => {
      clearTimeout(this.clipboardTimer);
      this.clipboardTimer = setTimeout(() => {
        this.clipboardTimer = 0;
        actions.clipboard(this.clipboardBox.value);
      }, CLIPBOARD_SEND_MS);
    });
    clipboardPanel.append(this.clipboardBox);

    this.keyboard = new OnScreenKeyboard(actions.key);
    const clipboardButton = this.button('Clipboard', 'Remote clipboard', () => {
      clipboardPanel.hidden = !clipboardPanel.hidden;
      clipboardButton.classList.toggle('on', !clipboardPanel.hidden);
    });
    const keyboardButton = this.button('Keyboard', 'On-screen keyboard', () => {
      this.keyboard.element.hidden = !this.keyboard.element.hidden;
      if (this.keyboard.element.hidden) this.keyboard.release();
      keyboardButton.classList.toggle('on', !this.keyboard.element.hidden);
      actions.layout();
    });
    const typeButton = this.button('Type', "This device's own keyboard, to type on the remote screen", () => textEntry.toggle());
    const textEntry = new TextEntry(actions, (on) => typeButton.classList.toggle('on', on));
    // Only with a touch screen: a computer has its keyboard already.
    typeButton.hidden = navigator.maxTouchPoints === 0;
    this.soundButton = this.button('Sound', '', actions.toggleSound);
    this.bar.append(
      this.fullscreenButton,
      this.displaySelect,
      clipboardButton,
      this.soundButton,
      typeButton,
      keyboardButton,
      this.button('Stats', 'Latency overlay', actions.toggleStats),
    );
    root.append(handle, this.bar, clipboardPanel, textEntry.element, this.keyboard.element);

    document.addEventListener('fullscreenchange', () => {
      const on = document.fullscreenElement !== null;
      if (!on) navigator.keyboard?.unlock();
      this.fullscreenButton.classList.toggle('on', on);
    });
  }

  /** Height the on-screen keyboard takes at the bottom of the window. */
  get reservedBottom(): number {
    return this.keyboard.element.offsetHeight;
  }

  /** The remote clipboard changed. */
  remoteClipboard(text: string): void {
    // Text being typed here is newer than what comes back.
    if (this.clipboardTimer === 0 && this.clipboardBox.value !== text) this.clipboardBox.value = text;
  }

  /** Whether the remote session's sound plays, and the reason when it does not. */
  sound(playing: boolean, status: string): void {
    this.soundButton.classList.toggle('on', playing);
    this.soundButton.title = `Sound of the remote session: ${status}`;
  }

  displays(list: DisplayEntry[], current: string | null): void {
    this.displaySelect.replaceChildren(
      ...list.map((d) => {
        const option = document.createElement('option');
        option.value = d.id;
        option.textContent = `${d.name || d.id} (${d.width}×${d.height})`;
        option.selected = d.id === current;
        return option;
      }),
    );
  }

  private button(label: string, title: string, onClick: () => void): HTMLButtonElement {
    const b = document.createElement('button');
    b.textContent = label;
    b.title = title;
    // Never focused: keys keep going to the remote screen.
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
    await navigator.keyboard?.lock().catch(() => {});
  }
}
