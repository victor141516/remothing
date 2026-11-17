// The virtual displays panel of the control bar: the streamed virtual
// display's mode (where the server can change it) and its removal, and a new
// display, by hand or of this window's size. Where the server needs a driver
// first (Windows), the user is told what will be installed and asked.

import type { DisplayMode, DisplaysMessage, UiActions, VirtualRequest } from './ui';

type ButtonFactory = (label: string, title: string, onClick: () => void) => HTMLButtonElement;

/** Smallest size a virtual display takes (proto.rs `DisplayMode::check`). */
const MIN_SIZE = 64;

export class DisplayPanel {
  readonly element = document.createElement('div');
  private message: DisplaysMessage | null = null;
  /** A request waiting for the installation the user agreed to. */
  private pending: VirtualRequest | null = null;
  private status = '';

  constructor(
    private readonly actions: UiActions,
    private readonly ask: (question: string) => Promise<boolean>,
    private readonly button: ButtonFactory,
  ) {
    this.element.id = 'ui-displays';
    this.element.hidden = true;
  }

  /** Virtual displays exist on this server. */
  get available(): boolean {
    return this.message?.virtual != null;
  }

  update(message: DisplaysMessage): void {
    this.message = message;
    if (message.error) {
      this.pending = null;
      this.status = '';
    } else if (this.pending && message.virtual?.setup.state === 'ready') {
      // Installed: what the user asked for in the first place.
      const request = this.pending;
      this.pending = null;
      this.status = 'Installed.';
      this.actions.virtualDisplay(request);
    }
    this.render();
  }

  private send(request: VirtualRequest): void {
    const setup = this.message?.virtual?.setup;
    if (setup?.state !== 'install') {
      this.status = '';
      this.actions.virtualDisplay(request);
      return;
    }
    void this.ask(`${setup.notice ?? ''} Install it now?`).then((yes) => {
      if (!yes) return;
      this.pending = request;
      this.status = 'Installing…';
      this.render();
      this.actions.virtualDisplay({ op: 'install' });
    });
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
    const m = this.message;
    const parts: HTMLElement[] = [];
    const virt = m?.virtual;
    if (!m || !virt) {
      this.element.replaceChildren();
      return;
    }
    if (virt.setup.state === 'unavailable') {
      parts.push(paragraph(`Virtual displays are not available: ${virt.setup.reason ?? ''}`));
    } else {
      const current = m.displays.find((d) => d.id === m.current && d.mode);
      if (current?.mode) {
        const id = current.id;
        parts.push(heading(`This display: ${current.name}`));
        if (virt.capabilities.resize) {
          parts.push(this.modeForm(current.mode, 'Apply', (mode) => this.send({ op: 'set', id, mode })));
        }
        parts.push(row(this.button('Remove', 'Removes this virtual display', () => this.send({ op: 'destroy', id }))));
      }
      parts.push(heading('New virtual display'));
      const size = this.fit();
      const initial = { ...size, refresh_hz: this.nearestRate(virt.capabilities.refresh_rates), hdr: false };
      parts.push(this.modeForm(initial, 'Create', (mode) => this.send({ op: 'create', mode })));
    }
    if (m.error) parts.push(paragraph(m.error, 'error'));
    else if (this.status) parts.push(paragraph(this.status));
    this.element.replaceChildren(...parts);
  }

  /** Width, height, refresh rate and HDR where offered, with "Fit window" and `submit`. */
  private modeForm(initial: DisplayMode, submit: string, onSubmit: (mode: DisplayMode) => void): HTMLElement {
    const caps = this.message?.virtual?.capabilities;
    const number = (value: number, max: number, title: string) => {
      const input = document.createElement('input');
      input.type = 'number';
      input.min = String(MIN_SIZE);
      input.max = String(max);
      input.step = '2';
      input.value = String(value);
      input.title = title;
      return input;
    };
    const width = number(initial.width, caps?.max_width ?? initial.width, 'Width, pixels');
    const height = number(initial.height, caps?.max_height ?? initial.height, 'Height, pixels');
    const parts: (HTMLElement | string)[] = [width, '×', height];
    const rates = caps?.refresh_rates ?? [];
    const rate = document.createElement('select');
    for (const r of rates) {
      const option = document.createElement('option');
      option.value = String(r);
      option.textContent = `${r} Hz`;
      option.selected = r === initial.refresh_hz;
      rate.append(option);
    }
    if (rates.length) parts.push(rate);
    const hdr = document.createElement('input');
    hdr.type = 'checkbox';
    hdr.checked = initial.hdr;
    if (caps?.hdr) {
      const label = document.createElement('label');
      label.append(hdr, ' HDR');
      parts.push(label);
    }
    parts.push(
      this.button('Fit window', "This window's size: shown without any scaling", () => {
        const s = this.fit();
        width.value = String(s.width);
        height.value = String(s.height);
      }),
      this.button(submit, '', () =>
        onSubmit({
          width: Number(width.value) & ~1,
          height: Number(height.value) & ~1,
          refresh_hz: rates.length ? Number(rate.value) : 0,
          hdr: hdr.checked,
        }),
      ),
    );
    return row(...parts);
  }
}

function heading(text: string): HTMLElement {
  const h = document.createElement('h3');
  h.textContent = text;
  return h;
}

function paragraph(text: string, className = ''): HTMLElement {
  const p = document.createElement('p');
  p.textContent = text;
  p.className = className;
  return p;
}

function row(...children: (Node | string)[]): HTMLElement {
  const div = document.createElement('div');
  div.className = 'row';
  div.append(...children);
  return div;
}
