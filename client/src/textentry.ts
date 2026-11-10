// The phone's own keyboard. Nothing on the page asks for it (the remote
// screen is a picture), so a text field nobody sees takes the focus, and
// what is typed into it is typed on the remote side.
//
// A phone's keyboard gives no usable key events: it edits the field
// (composes a word letter by letter, replaces it with a suggestion, writes a
// swiped word at once). So the field is compared with what it held before:
// what went away is sent as Backspace presses, what is new as text. Some
// filler always stays before the caret, because Backspace in an empty field
// reports nothing.

import { EVDEV } from './keymap';

/** Spaces: a keyboard starts a new word after them, and never joins what is typed with the filler. */
const FILLER = ' '.repeat(8);

export interface TextEntryActions {
  key(evdev: number, down: boolean): void;
  /** Types characters on the remote side. */
  text(text: string): void;
}

export class TextEntry {
  readonly element = document.createElement('textarea');
  private last = FILLER;
  private composing = false;

  /** `focused`: the field got or lost the focus (the keyboard follows it). */
  constructor(
    private readonly actions: TextEntryActions,
    focused: (on: boolean) => void,
  ) {
    const field = this.element;
    field.id = 'ui-text';
    // The remote side gets what was typed, not what the phone would make of it.
    field.autocapitalize = 'off';
    field.autocomplete = 'off';
    field.spellcheck = false;
    field.setAttribute('autocorrect', 'off');
    field.addEventListener('focus', () => {
      this.refill();
      focused(true);
    });
    field.addEventListener('blur', () => focused(false));
    field.addEventListener('compositionstart', () => (this.composing = true));
    field.addEventListener('compositionend', () => {
      this.composing = false;
      this.sync();
    });
    field.addEventListener('input', () => this.sync());
    // A real keyboard (attached to the phone, or a computer's) sends keys;
    // the phone's own has no key codes and goes through `sync`.
    for (const [type, down] of [['keydown', true], ['keyup', false]] as const) {
      field.addEventListener(type, (e) => {
        const code = EVDEV[e.code];
        if (code === undefined) return;
        e.preventDefault();
        actions.key(code, down);
      });
    }
  }

  /** Opens the keyboard, or closes it when it has the focus. */
  toggle(): void {
    if (document.activeElement === this.element) this.element.blur();
    else this.element.focus({ preventScroll: true });
  }

  /** Sends what changed in the field since the last time. */
  private sync(): void {
    const now = this.element.value;
    let same = 0;
    while (same < now.length && same < this.last.length && now[same] === this.last[same]) same++;
    // Not between the two halves of one character (an emoji replaced by another).
    if (same > 0 && (now.charCodeAt(same - 1) & 0xfc00) === 0xd800) same--;
    for (const _ of this.last.slice(same)) {
      this.actions.key(EVDEV.Backspace, true);
      this.actions.key(EVDEV.Backspace, false);
    }
    if (now.length > same) this.actions.text(now.slice(same));
    this.last = now;
    // A word being composed stays: the keyboard is still changing it.
    if (!this.composing) this.refill();
  }

  private refill(): void {
    if (this.element.value !== FILLER) this.element.value = FILLER;
    this.element.setSelectionRange(FILLER.length, FILLER.length);
    this.last = FILLER;
  }
}
