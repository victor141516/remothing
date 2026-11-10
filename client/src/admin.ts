// Admin UI for the JSON API in crates/core/src/admin.rs. Dependency-free: the
// DOM is built with `h()` (text nodes only, so server strings are never parsed as HTML).

type Pair = [number, number] | null;

interface DisplayInfo {
  id: string;
  name: string;
  width: number;
  height: number;
  refresh_hz: number;
  x: number;
  y: number;
  primary: boolean;
  kind: 'real' | 'virtual';
}

interface ClientStatus {
  id: number;
  addr: string;
  connected_ms: number;
  encoder: string | null;
  encoder_fallback: string | null;
  fps: number;
  mbps: number;
  link_mbps: number;
  link_proven: boolean;
  rtt_ms: number;
  video_cap_mbps: number | null;
  transport: string;
  lost_frames: number;
  paused: boolean;
  client_report: Record<string, Pair> | null;
}

interface Status {
  platform: string;
  provider: string;
  display: DisplayInfo | null;
  /** The screen moves as a whole and is streamed as video (motion mode). */
  motion: boolean;
  encoders: string[];
  /** Compiled-in backends that failed the startup probe: [name, reason]. */
  encoders_unavailable: [string, string][];
  encoder_preferred: string;
  gpus: { name: string; vendor: string; utilization: number | null }[];
  cpu_percent: number;
  /** What plays to the clients, or what was tried to capture the sound. */
  sound: string;
  /** Who has the session's microphone, and where it plays. */
  microphone: string;
  clients: ClientStatus[];
  errors: { at_ms: number; context: string; message: string }[];
  /** What happened to each client's stream (path switches, cuts, losses, stutters). */
  events: { at_ms: number; client: number; text: string }[];
}

interface Displays {
  displays: DisplayInfo[];
  current: string | null;
  can_create_virtual: boolean;
}

interface Tuning {
  bandwidth_mbps: number;
  max_lossless_frame_kb: number;
}

interface Settings {
  tuning: Tuning;
  codec: string;
  codecs: string[];
  encoder_preferred: string;
  encoders: string[];
  keyboard_layout: string;
  stream_token: string;
  admin_password_set: boolean;
  tls: boolean;
  config_path: string;
  stream_listen: string;
}

type Tab = 'status' | 'displays' | 'settings';

// ---------------------------------------------------------------- DOM helpers

type Child = Node | string | number | null | undefined | false;
type Attrs = Record<string, string | boolean | number | ((e: Event) => void)>;

function h(tag: string, attrs: Attrs = {}, ...children: Child[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (typeof v === 'boolean') {
      if (v) el.setAttribute(k, '');
    } else el.setAttribute(k, String(v));
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : String(c));
  }
  return el;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function table(headers: string[], rows: Node[][], empty: string): HTMLElement {
  if (!rows.length) return h('p', { class: 'dim' }, empty);
  return h(
    'table',
    {},
    h('thead', {}, h('tr', {}, ...headers.map((t) => h('th', {}, t)))),
    h('tbody', {}, ...rows.map((r) => h('tr', {}, ...r))),
  );
}

const td = (...c: Child[]) => h('td', {}, ...c);
const tdNum = (...c: Child[]) => h('td', { class: 'num' }, ...c);
const badge = (cls: string, text: string) => h('span', { class: `badge ${cls}` }, text);

function bar(pct: number): HTMLElement {
  const fill = h('i');
  fill.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  return h('span', { class: 'bar' }, fill);
}

const fmtPair = (p: Pair | undefined) => (p ? `${p[0].toFixed(1)} / ${p[1].toFixed(1)}` : '—');
const fmtRes = (d: DisplayInfo) => `${d.width}×${d.height}`;
const fmtHz = (hz: number) => (hz > 0 ? `${+hz.toFixed(2)} Hz` : '—');

function fmtAge(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

// ---------------------------------------------------------------- API

class AuthError extends Error {}

async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) {
    showLogin('Session expired or not logged in.');
    throw new AuthError('login required');
  }
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} ${path}: ${res.status} ${text || res.statusText}`);
  return (text ? JSON.parse(text) : {}) as T;
}

// ---------------------------------------------------------------- banner

let bannerTimer = 0;

function banner(kind: 'error' | 'info' | 'warn', msg: string) {
  const el = $('banner');
  el.className = kind;
  el.textContent = msg;
  el.hidden = false;
  clearTimeout(bannerTimer);
  // Errors stay until replaced or dismissed by a successful action; confirmations fade.
  if (kind === 'info') bannerTimer = window.setTimeout(() => (el.hidden = true), 4000);
}

const clearBanner = () => ($('banner').hidden = true);

/** Runs a user action, reporting API failures in the banner (401 already switched to login). */
async function action(fn: () => Promise<void>) {
  try {
    await fn();
  } catch (e) {
    if (!(e instanceof AuthError)) banner('error', (e as Error).message);
  }
}

// ---------------------------------------------------------------- login

let loggedIn = false;

function showLogin(msg = '') {
  loggedIn = false;
  stopPolling();
  $('app-view').hidden = true;
  $('login-view').hidden = false;
  $('login-error').textContent = msg;
  $<HTMLInputElement>('login-secret').focus();
}

function showApp() {
  loggedIn = true;
  $('login-view').hidden = true;
  $('app-view').hidden = false;
  clearBanner();
  selectTab(currentTab);
}

function initLogin() {
  const form = $<HTMLFormElement>('login-form');
  const secret = $<HTMLInputElement>('login-secret');
  form.addEventListener('change', () => {
    secret.placeholder = kind() === 'token' ? 'Admin token' : 'Password';
  });
  const kind = () => (new FormData(form).get('kind') as string) ?? 'token';
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    $('login-error').textContent = '';
    const res = await fetch('api/login', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ [kind()]: secret.value }),
    }).catch((err: Error) => err);
    if (res instanceof Error) {
      $('login-error').textContent = `Network error: ${res.message}`;
    } else if (res.ok) {
      secret.value = '';
      showApp();
    } else {
      $('login-error').textContent = res.status === 401 ? 'Invalid credentials.' : `${res.status} ${await res.text()}`;
    }
  });
  $('logout').addEventListener('click', () =>
    action(async () => {
      await api('POST', 'api/logout');
      showLogin('Logged out.');
    }),
  );
}

// ---------------------------------------------------------------- tabs

let currentTab: Tab = 'status';
let pollTimer = 0;
let pollGen = 0;

function stopPolling() {
  clearTimeout(pollTimer);
  pollGen++;
}

function selectTab(tab: Tab) {
  currentTab = tab;
  stopPolling();
  for (const b of document.querySelectorAll<HTMLButtonElement>('nav button')) {
    b.classList.toggle('active', b.dataset.tab === tab);
  }
  for (const t of ['status', 'displays', 'settings'] as Tab[]) $(`tab-${t}`).hidden = t !== tab;
  if (tab === 'status') pollStatus(pollGen);
  else if (tab === 'displays') void action(loadDisplays);
  else void action(loadSettings);
}

// ---------------------------------------------------------------- status

/** Sequential 1 s poll: the next request starts after the previous one finished, so slow responses never pile up. */
async function pollStatus(gen: number) {
  try {
    const s = await api<Status>('GET', 'api/status');
    if (gen !== pollGen) return;
    renderStatus(s);
  } catch (e) {
    if (e instanceof AuthError) return;
    if (gen === pollGen) banner('error', (e as Error).message);
  }
  if (gen === pollGen && loggedIn) pollTimer = window.setTimeout(() => pollStatus(gen), 1000);
}

const LATENCIES: [string, string][] = [
  ['captureToDraw', 'capture→draw'],
  ['appToDraw', 'app→draw'],
  ['inputRtt', 'input rtt'],
  ['network', 'network'],
  ['decode', 'decode'],
];

function renderStatus(s: Status) {
  const d = s.display;
  const overview = h(
    'section',
    {},
    h('h2', {}, 'Server'),
    h(
      'dl',
      {},
      h('dt', {}, 'Platform'),
      h('dd', {}, s.platform),
      h('dt', {}, 'Display provider'),
      h('dd', {}, s.provider),
      h('dt', {}, 'Current display'),
      h('dd', {}, d ? `${d.name} (${d.id}) ${fmtRes(d)} @ ${fmtHz(d.refresh_hz)}` : h('span', { class: 'dim' }, 'none')),
      h('dt', {}, 'Mode'),
      h('dd', {}, s.motion ? 'motion (the screen moves as a whole: what changes goes as video)' : 'tiles'),
      h('dt', {}, 'Encoders'),
      h('dd', {}, s.encoders.length ? s.encoders.join(', ') : h('span', { class: 'dim' }, 'none (lossless tiles only)')),
      h('dt', {}, 'Unavailable encoders'),
      h(
        'dd',
        {},
        s.encoders_unavailable.length
          ? h('ul', { class: 'plain' }, ...s.encoders_unavailable.map(([name, why]) => h('li', {}, h('b', {}, name), ': ', why)))
          : h('span', { class: 'dim' }, 'none'),
      ),
      h('dt', {}, 'Preferred encoder'),
      h('dd', {}, s.encoder_preferred),
      h('dt', {}, 'CPU'),
      h('dd', {}, bar(s.cpu_percent), `${s.cpu_percent.toFixed(1)} %`),
      h('dt', {}, 'Sound'),
      h('dd', {}, s.sound),
      h('dt', {}, 'Microphone'),
      h('dd', {}, s.microphone),
    ),
  );

  const gpus = h(
    'section',
    {},
    h('h2', {}, 'GPUs'),
    table(
      ['Name', 'Vendor', 'Utilization'],
      s.gpus.map((g) => [
        td(g.name),
        td(g.vendor),
        td(g.utilization === null ? h('span', { class: 'dim' }, 'n/a') : h('span', {}, bar(g.utilization), `${g.utilization.toFixed(0)} %`)),
      ]),
      'No GPU detected.',
    ),
  );

  const clients = h(
    'section',
    {},
    h('h2', {}, 'Clients'),
    table(
      ['#', 'Address', 'Connected', 'Transport', 'Encoder', 'FPS', 'Mbit/s', 'Link Mbit/s', 'RTT ms', 'Video cap Mbit/s', ...LATENCIES.map(([, label]) => `${label} p50/p99 ms`)],
      s.clients.map((c) => [
        tdNum(String(c.id)),
        td(h('span', { class: 'mono' }, c.addr)),
        td(fmtAge(c.connected_ms)),
        td(
          c.transport === 'webrtc' ? `WebRTC (UDP), ${c.lost_frames} lost` : 'WebSocket',
          c.paused && h('div', { class: 'dim' }, 'paused (tab hidden)'),
        ),
        td(
          c.encoder ?? h('span', { class: 'dim' }, 'lossless only'),
          c.encoder_fallback && h('div', { class: 'dim' }, `fallback: ${c.encoder_fallback}`),
        ),
        tdNum(c.fps.toFixed(1)),
        tdNum(c.mbps.toFixed(2)),
        tdNum(`${c.link_proven ? '' : '≥ '}${c.link_mbps.toFixed(1)}`),
        tdNum(c.rtt_ms.toFixed(1)),
        tdNum(c.video_cap_mbps === null ? '—' : c.video_cap_mbps.toFixed(1)),
        ...LATENCIES.map(([key]) => tdNum(fmtPair(c.client_report?.[key]))),
      ]),
      'No clients connected.',
    ),
  );

  const errors = h(
    'section',
    {},
    h('h2', {}, `Recent errors (${s.errors.length})`),
    table(
      ['Time', 'Context', 'Message'],
      s.errors
        .slice()
        .reverse()
        .map((e) => [
          td(new Date(e.at_ms).toLocaleTimeString()),
          td(e.context),
          td(h('span', { class: 'mono err' }, e.message)),
        ]),
      'No errors.',
    ),
  );

  const events = h(
    'section',
    {},
    h('h2', {}, 'Recent events'),
    table(
      ['Time', 'Client', 'Event'],
      s.events
        .slice(-60)
        .reverse()
        .map((e) => [td(new Date(e.at_ms).toLocaleTimeString()), tdNum(String(e.client)), td(e.text)]),
      'No events.',
    ),
  );

  $('tab-status').replaceChildren(overview, gpus, clients, events, errors);
}

// ---------------------------------------------------------------- displays

async function loadDisplays() {
  const d = await api<Displays>('GET', 'api/displays');
  if (currentTab === 'displays') renderDisplays(d);
}

function renderDisplays(d: Displays) {
  const rows = d.displays.map((x) => {
    const current = x.id === d.current;
    return [
      td(h('span', { class: 'mono' }, x.id)),
      td(x.name),
      tdNum(fmtRes(x)),
      tdNum(fmtHz(x.refresh_hz)),
      tdNum(`${x.x}, ${x.y}`),
      td(x.primary ? 'yes' : ''),
      td(badge(x.kind, x.kind), ' ', current && badge('current', 'streaming')),
      td(
        h('div', { class: 'row' },
          h('button', {
            disabled: current,
            onclick: () =>
              action(async () => {
                await api('POST', 'api/displays/select', { id: x.id });
                banner('info', `Streaming ${x.name}.`);
                await loadDisplays();
              }),
          }, 'Stream this'),
          x.kind === 'virtual' && d.can_create_virtual &&
            h('button', {
              class: 'danger',
              onclick: () =>
                action(async () => {
                  if (!confirm(`Destroy virtual display ${x.name}?`)) return;
                  await api('DELETE', `api/virtual/${encodeURIComponent(x.id)}`);
                  banner('info', `Destroyed ${x.name}.`);
                  await loadDisplays();
                }),
            }, 'Delete'),
        ),
      ),
    ];
  });

  const list = h(
    'section',
    {},
    h('div', { class: 'row' }, h('h2', { style: 'flex:1' }, 'Displays'), h('button', { onclick: () => action(loadDisplays) }, 'Refresh')),
    table(['Id', 'Name', 'Resolution', 'Refresh', 'Position', 'Primary', 'Kind', ''], rows, 'No displays found.'),
  );

  const sections: HTMLElement[] = [list];
  if (d.can_create_virtual) {
    const width = h('input', { type: 'number', min: 64, max: 7680, value: 1920, required: true }) as HTMLInputElement;
    const height = h('input', { type: 'number', min: 64, max: 4320, value: 1080, required: true }) as HTMLInputElement;
    const form = h(
      'form',
      { class: 'row' },
      'Width', width, 'Height', height,
      h('button', { class: 'primary', type: 'submit' }, 'Create virtual display'),
    );
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      void action(async () => {
        const info = await api<DisplayInfo>('POST', 'api/virtual', { width: +width.value, height: +height.value });
        banner('info', `Created ${info.name} (${info.id}) ${fmtRes(info)}.`);
        await loadDisplays();
      });
    });
    sections.push(h('section', {}, h('h2', {}, 'New virtual display'), form));
  }
  $('tab-displays').replaceChildren(...sections);
}

// ---------------------------------------------------------------- settings

function randomToken(): string {
  // Same shape as the server's config::random_token (32 alphanumerics).
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  const out: string[] = [];
  const buf = new Uint8Array(64);
  while (out.length < 32) {
    crypto.getRandomValues(buf);
    // Rejection sampling keeps the distribution uniform (248 = 4 * 62).
    for (const b of buf) if (b < 248 && out.length < 32) out.push(alphabet[b % 62]);
  }
  return out.join('');
}

function streamLink(s: Settings): string {
  const scheme = s.tls ? 'https' : 'http';
  const sep = s.stream_listen.lastIndexOf(':');
  let host = s.stream_listen.slice(0, sep).replace(/^\[|\]$/g, '');
  const port = s.stream_listen.slice(sep + 1);
  // Wildcard binds are reachable on whatever host this admin page was opened with.
  if (host === '0.0.0.0' || host === '::' || host === '') host = location.hostname;
  if (host.includes(':') && !host.startsWith('[')) host = `[${host}]`;
  const token = s.stream_token ? `/?token=${encodeURIComponent(s.stream_token)}` : '/';
  return `${scheme}://${host}:${port}${token}`;
}

async function loadSettings() {
  const s = await api<Settings>('GET', 'api/settings');
  if (currentTab === 'settings') renderSettings(s);
}

async function saveSettings(patch: Record<string, unknown>, what: string) {
  const r = await api<{ ok: boolean; warnings?: string[] }>('PUT', 'api/settings', patch);
  if (r.warnings?.length) banner('warn', `${what} saved with warnings:\n${r.warnings.join('\n')}`);
  else banner('info', `${what} saved.`);
  await loadSettings();
}

function renderSettings(s: Settings) {
  const field = (label: string, ...input: Child[]) => [h('label', {}, label), h('div', { class: 'row' }, ...input)];

  const bandwidth = h('input', { type: 'number', min: 0.1, step: 'any', required: true, value: s.tuning.bandwidth_mbps }) as HTMLInputElement;
  const maxKb = h('input', { type: 'number', min: 1, step: 1, required: true, value: s.tuning.max_lossless_frame_kb }) as HTMLInputElement;
  // The server only builds the codecs it lists; with a single one there is nothing to choose.
  const codec = h('select', { disabled: s.codecs.length < 2 }, ...s.codecs.map((c) => h('option', { value: c, selected: c === s.codec }, c))) as HTMLSelectElement;
  const encoder = h(
    'select',
    {},
    ...['auto', ...s.encoders].map((e) => h('option', { value: e, selected: e === s.encoder_preferred }, e)),
  ) as HTMLSelectElement;
  const layout = h('input', { type: 'text', value: s.keyboard_layout, placeholder: 'e.g. us, de, fr', list: 'layouts' }) as HTMLInputElement;
  const layouts = h('datalist', { id: 'layouts' }, ...['us', 'gb', 'de', 'fr', 'es', 'it', 'pt', 'ch', 'se', 'no', 'dk', 'fi', 'jp'].map((l) => h('option', { value: l })));

  const streaming = h(
    'form',
    { class: 'grid' },
    ...field('Bandwidth cap', bandwidth, h('span', { class: 'dim' }, 'Mbit/s — upper bound; each client\'s measured link usually decides')),
    ...field('Max lossless frame', maxKb, h('span', { class: 'dim' }, 'KB — larger moving areas go to video')),
    ...field('Codec', codec),
    ...field('Encoder preference', encoder, h('span', { class: 'dim' }, s.encoders.length ? '' : 'no hardware/software encoder available')),
    ...field('Keyboard layout', layout, layouts),
    h('span'),
    h('div', {}, h('button', { class: 'primary', type: 'submit' }, 'Save')),
  );
  streaming.addEventListener('submit', (e) => {
    e.preventDefault();
    void action(() =>
      saveSettings(
        {
          tuning: { bandwidth_mbps: +bandwidth.value, max_lossless_frame_kb: Math.round(+maxKb.value) },
          encoder_preferred: encoder.value,
          keyboard_layout: layout.value.trim(),
        },
        'Streaming settings',
      ),
    );
  });

  const link = streamLink(s);
  const access = h(
    'dl',
    {},
    h('dt', {}, 'Stream token'),
    h('dd', {}, h('div', { class: 'row' },
      h('span', { class: 'mono' }, s.stream_token || '(empty — stream auth disabled)'),
      h('button', {
        onclick: () =>
          action(async () => {
            if (!confirm('Generate a new stream token? Clients using the old link will be rejected on reconnect.')) return;
            await saveSettings({ stream_token: randomToken() }, 'Stream token');
          }),
      }, 'Regenerate'),
    )),
    h('dt', {}, 'Client link'),
    h('dd', {}, h('div', { class: 'row' },
      h('a', { class: 'mono', href: link, target: '_blank', rel: 'noopener' }, link),
      h('button', { onclick: () => action(() => navigator.clipboard.writeText(link).then(() => banner('info', 'Link copied.'))) }, 'Copy'),
    )),
  );

  const pw1 = h('input', { type: 'password', autocomplete: 'new-password', required: true, minlength: 8 }) as HTMLInputElement;
  const pw2 = h('input', { type: 'password', autocomplete: 'new-password', required: true }) as HTMLInputElement;
  const password = h(
    'form',
    { class: 'grid' },
    h('span', { class: 'dim' }, 'Current'),
    h('div', {}, s.admin_password_set ? 'set' : h('span', { class: 'dim' }, 'not set (token login only)')),
    ...field('New password', pw1),
    ...field('Repeat', pw2),
    h('span'),
    h('div', {}, h('button', { class: 'primary', type: 'submit' }, 'Set password')),
  );
  password.addEventListener('submit', (e) => {
    e.preventDefault();
    if (pw1.value !== pw2.value) {
      banner('error', 'Passwords do not match.');
      return;
    }
    void action(() => saveSettings({ admin_password: pw1.value }, 'Admin password'));
  });

  const server = h(
    'dl',
    {},
    h('dt', {}, 'Config file'),
    h('dd', { class: 'mono' }, s.config_path),
    h('dt', {}, 'TLS'),
    h('dd', {}, s.tls ? badge('real', 'enabled') : badge('virtual', 'disabled (plain HTTP)')),
    h('dt', {}, 'Stream listener'),
    h('dd', { class: 'mono' }, s.stream_listen),
  );

  $('tab-settings').replaceChildren(
    h('section', {}, h('h2', {}, 'Streaming'), streaming),
    h('section', {}, h('h2', {}, 'Client access'), access),
    h('section', {}, h('h2', {}, 'Admin password'), password),
    h('section', {}, h('h2', {}, 'Server'), server),
  );
}

// ---------------------------------------------------------------- boot

initLogin();
for (const b of document.querySelectorAll<HTMLButtonElement>('nav button')) {
  b.addEventListener('click', () => {
    clearBanner();
    selectTab(b.dataset.tab as Tab);
  });
}

// An existing session cookie skips the login form.
fetch('api/status', { credentials: 'same-origin' })
  .then((r) => (r.ok ? showApp() : showLogin()))
  .catch((e: Error) => showLogin(`Server unreachable: ${e.message}`));
