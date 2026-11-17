// The Displays page of the controls menu: every display of the remote
// computer as one list (a row is the choice of what this window shows), the
// mode of the virtual display shown here (where the server can change it),
// each virtual display's removal, and a new one, by hand or of this window's
// size. Where the server needs a driver first (Windows), the user is told
// what will be installed and asked.

import type { DisplayEntry, DisplayMode, DisplaysMessage, UiActions, VirtualRequest } from './ui';

type ButtonFactory = (label: string, title: string, onClick: () => void) => HTMLButtonElement;

/** Smallest size a virtual display takes (proto.rs `DisplayMode::check`). */
const MIN_SIZE = 64;

export class DisplaysPage {
  /** The page's own part below its header: the list, the forms, notes. */
  readonly body = document.createElement('div');
  /** The status line in the page header (also mirrored on the Display tile). */
  readonly status = document.createElement('p');
  private message: DisplaysMessage | null = null;
  /** A request waiting for the installation the user agreed to. */
  private pending: VirtualRequest | null = null;
  private statusText = '';
  /** What was typed into the new display's form: the list may arrive again
   * meanwhile (another window changed the displays). */
  private draft: DisplayMode | null = null;
  private newOpen = false;
  /** A list arrived while a field of the page had the focus: shown once it leaves. */
  private stale = false;

  /** `changed`: the status line or the display shown here changed (the Display tile follows). */
  constructor(
    private readonly actions: UiActions,
    private readonly ask: (question: string) => Promise<boolean>,
    private readonly button: ButtonFactory,
    private readonly changed: () => void,
    /** After a choice made in a field (select, checkbox, form): focus back to the picture. */
    private readonly toPicture: () => void,
  ) {
    this.status.className = 'status';
    this.status.setAttribute('role', 'status');
    // Rebuilt only when nothing in it is being typed into.
    this.body.addEventListener('focusout', (e) => {
      if (this.stale && !this.body.contains(e.relatedTarget as Node | null)) this.render();
    });
  }

  /** The display this window shows, once known. */
  get current(): DisplayEntry | undefined {
    const m = this.message;
    return m?.displays.find((d) => d.id === m.current);
  }

  /** The status line: error (true) or progress, or nothing. */
  get line(): { text: string; error: boolean } {
    const error = this.message?.error;
    return error ? { text: `⚠ ${error}`, error: true } : { text: this.statusText, error: false };
  }

  update(message: DisplaysMessage): void {
    this.message = message;
    if (message.error) {
      this.pending = null;
      this.statusText = '';
    } else if (this.pending && message.virtual?.setup.state === 'ready') {
      // Installed: what the user asked for in the first place.
      const request = this.pending;
      this.pending = null;
      this.statusText = 'Installed.';
      this.actions.virtualDisplay(request);
    }
    this.render();
  }

  /** The page was left: the new display's form folds. */
  fold(): void {
    if (!this.newOpen) return;
    this.newOpen = false;
    this.render();
  }

  private send(request: VirtualRequest): void {
    const setup = this.message?.virtual?.setup;
    if (setup?.state !== 'install') {
      this.statusText = '';
      this.actions.virtualDisplay(request);
      this.showStatus();
      return;
    }
    void this.ask(`${setup.notice ?? ''} Install it now?`).then((yes) => {
      if (!yes) return;
      this.pending = request;
      this.statusText = 'Installing…';
      this.showStatus();
      this.actions.virtualDisplay({ op: 'install' });
    });
  }

  private showStatus(): void {
    const { text, error } = this.line;
    this.status.textContent = text;
    this.status.classList.toggle('error', error);
    this.changed();
  }

  /** This window's size, within what the server takes (even, for the video). */
  private fit(): { width: number; height: number } {
    const caps = this.message?.virtual?.capabilities;
    const { width, height } = this.actions.viewport();
    const clamp = (v: number, max: number) => Math.max(MIN_SIZE, Math.min(max, v)) & ~1;
    return { width: clamp(width, caps?.max_width ?? width), height: clamp(height, caps?.max_height ?? height) };
  }

  /** The offered refresh rate closest to this screen's. */
  private nearestRate(rates: number[]): number {
    const hz = this.actions.viewport().hz || 60;
    return rates.reduce((best, r) => (Math.abs(r - hz) < Math.abs(best - hz) ? r : best), rates[0] ?? 0);
  }

  private render(): void {
    if (this.body.contains(document.activeElement)) {
      this.stale = true;
      this.showStatus();
      return;
    }
    this.stale = false;
    this.showStatus();
    const m = this.message;
    if (!m) {
      this.body.replaceChildren(note('Waiting for the list…'));
      return;
    }
    const virt = m.virtual;
    const usable = virt && virt.setup.state !== 'unavailable' ? virt : null;
    const list = document.createElement('div');
    list.className = 'list';
    list.setAttribute('role', 'radiogroup');
    for (const d of m.displays) {
      const here = d.id === m.current;
      const pick = this.button('', 'Show it in this window', () => {
        if (!here) this.actions.selectDisplay(d.id);
      });
      pick.className = 'pick';
      pick.setAttribute('role', 'radio');
      pick.setAttribute('aria-checked', String(here));
      const extra = [d.mode?.refresh_hz ? `${d.mode.refresh_hz} Hz` : '', d.mode?.hdr ? 'HDR' : ''].filter(Boolean);
      pick.append(
        span('mark', here ? '◉' : '○'),
        span('name', d.name || d.id),
        ...(d.kind === 'virtual' ? [span('tag', 'virtual')] : []),
        span('size', [`${d.width}×${d.height}`, ...extra].join(' · ')),
      );
      const row = document.createElement('div');
      row.className = here ? 'display current' : 'display';
      row.append(pick);
      // Only displays a client made can be removed (not the session's main one).
      if (d.mode) {
        const id = d.id;
        const remove = this.button('Remove', 'Removes this virtual display (windows showing it go to the default display)', () =>
          this.send({ op: 'destroy', id }),
        );
        remove.classList.add('remove');
        row.append(remove);
      }
      list.append(row);
      if (here && d.mode && usable?.capabilities.resize) {
        const id = d.id;
        list.append(this.modeForm(d.mode, 'Apply', (mode) => this.send({ op: 'set', id, mode })));
      }
    }
    const parts: HTMLElement[] = [heading('Shown in this window'), list];
    if (usable) {
      const disclose = this.button(`${this.newOpen ? '－' : '＋'} New virtual display`, '', () => {
        this.newOpen = !this.newOpen;
        this.render();
      });
      disclose.className = 'disclose';
      disclose.setAttribute('aria-expanded', String(this.newOpen));
      parts.push(disclose);
      if (this.newOpen) {
        const initial = this.draft ?? { ...this.fit(), refresh_hz: this.nearestRate(usable.capabilities.refresh_rates), hdr: false };
        const form = this.modeForm(initial, 'Create', (mode) => this.send({ op: 'create', mode }), (mode) => (this.draft = mode));
        if (usable.setup.state === 'install') {
          form.append(note('Needs a driver on the remote computer. You are asked before it is installed.'));
        }
        parts.push(form);
      }
    } else if (virt) {
      parts.push(note(`Virtual displays are not available: ${virt.setup.reason ?? ''}`));
    }
    this.body.replaceChildren(...parts);
  }

  /** Width, height, refresh rate and HDR where offered, with "Fit window"
   * and `submit`; `edited` hears every change. */
  private modeForm(
    initial: DisplayMode,
    submit: string,
    onSubmit: (mode: DisplayMode) => void,
    edited: (mode: DisplayMode) => void = () => {},
  ): HTMLFormElement {
    const caps = this.message?.virtual?.capabilities;
    const form = document.createElement('form');
    form.className = 'mode';
    const number = (value: number, max: number) => {
      const input = document.createElement('input');
      input.type = 'number';
      input.min = String(MIN_SIZE);
      input.max = String(max);
      input.step = '2';
      input.inputMode = 'numeric';
      input.value = String(value);
      return input;
    };
    const width = number(initial.width, caps?.max_width ?? initial.width);
    const height = number(initial.height, caps?.max_height ?? initial.height);
    const x = span('x', '×');
    form.append(labelled('Width', width), x, labelled('Height', height));
    const rates = caps?.refresh_rates ?? [];
    const rate = document.createElement('select');
    for (const r of rates) {
      const option = document.createElement('option');
      option.value = String(r);
      option.textContent = `${r} Hz`;
      option.selected = r === initial.refresh_hz;
      rate.append(option);
    }
    if (rates.length) form.append(labelled('Rate', rate));
    const hdr = document.createElement('input');
    hdr.type = 'checkbox';
    hdr.checked = initial.hdr;
    if (caps?.hdr) {
      const label = document.createElement('label');
      label.className = 'check';
      label.append(hdr, ' HDR');
      form.append(label);
    }
    const read = (): DisplayMode => ({
      width: Number(width.value) & ~1,
      height: Number(height.value) & ~1,
      refresh_hz: rates.length ? Number(rate.value) : 0,
      hdr: hdr.checked,
    });
    for (const input of [width, height, rate, hdr]) input.addEventListener('input', () => edited(read()));
    // One-gesture fields hand the keys back to the picture at once.
    for (const input of [rate, hdr]) input.addEventListener('change', this.toPicture);
    const fit = this.button('Fit window', "This window's size: shown without any scaling", () => {
      const s = this.fit();
      width.value = String(s.width);
      height.value = String(s.height);
      edited(read());
    });
    fit.type = 'button';
    const go = this.button(submit, '', () => {});
    go.type = 'submit';
    go.classList.add('primary');
    const actions = document.createElement('div');
    actions.className = 'actions';
    actions.append(fit, go);
    form.append(actions);
    // Enter in a field submits too.
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      this.toPicture();
      onSubmit(read());
    });
    return form;
  }
}

function heading(text: string): HTMLElement {
  const h = document.createElement('h3');
  h.textContent = text;
  return h;
}

function note(text: string): HTMLElement {
  const p = document.createElement('p');
  p.className = 'note';
  p.textContent = text;
  return p;
}

function span(className: string, text: string): HTMLElement {
  const s = document.createElement('span');
  s.className = className;
  s.textContent = text;
  return s;
}

function labelled(text: string, field: HTMLElement): HTMLElement {
  const label = document.createElement('label');
  label.append(text, field);
  return label;
}
