// weft in a browser: the list of sessions, and one session's terminal.
//
// Everything a session shows is drawn by the weft running on the Mac; this
// page is a terminal (xterm.js) and a socket. Text from the server is only
// ever set with textContent.

'use strict';

const $ = (id) => document.getElementById(id);
const enc = new TextEncoder();

// ---- Home --------------------------------------------------------------------

async function api(path, opts) {
  const res = await fetch(path, { credentials: 'same-origin', ...opts });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.status === 204 ? null : res.json();
}

function ago(iso) {
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function row(name, meta, onOpen, dotLive) {
  const li = el('li');
  const open = el('button', 'open');
  const n = el('span', 'name');
  if (dotLive != null) n.append(el('span', dotLive ? 'dot live' : 'dot'));
  n.append(document.createTextNode(name));
  open.append(n, el('span', 'meta', meta));
  open.addEventListener('click', onOpen);
  li.append(open);
  return li;
}

async function showHome() {
  document.body.classList.remove('in-session');
  $('session').hidden = true;
  $('home').hidden = false;
  document.title = 'weft';
  try {
    const [sessions, folders] = await Promise.all([api('/api/sessions'), api('/api/folders')]);
    $('error').hidden = true;

    const list = $('sessions');
    list.replaceChildren();
    for (const s of sessions) {
      const meta = `${s.alive ? 'running' : 'ended'} · started ${ago(s.started)} · ${s.folder}`;
      const li = row(s.name, meta, () => (s.alive ? openSession({ id: s.id }, s.name) : null), s.alive);
      const end = el('button', 'end', s.alive ? 'end' : 'dismiss');
      end.addEventListener('click', async () => {
        if (s.alive && !confirm(`End the weft session in ${s.name}? A task in progress stops.`)) return;
        await api(`/api/sessions/${encodeURIComponent(s.id)}`, { method: 'DELETE' }).catch(fail);
        showHome();
      });
      li.append(end);
      list.append(li);
    }
    $('no-sessions').hidden = sessions.length > 0;

    const fl = $('folders');
    fl.replaceChildren();
    for (const f of folders) {
      const live = sessions.find((s) => s.alive && s.folder === f.path);
      fl.append(row(f.name, live ? 'running · tap to join' : f.path, () => openSession({ folder: f.path }, f.name)));
    }
  } catch (e) {
    fail(e);
  }
}

function fail(e) {
  $('error').textContent = String(e.message || e);
  $('error').hidden = false;
}

// ---- A session -----------------------------------------------------------------

const KEYS = [
  ['esc', '\x1b'],
  ['⇧tab', '\x1b[Z'],
  ['y', 'y'],
  ['a', 'a'],
  ['n', 'n'],
  ['↑', '\x1b[A'],
  ['↓', '\x1b[B'],
  ['/', '/'],
  ['^C', '\x03'],
  ['nl', '\n'], // ctrl-j: a newline in the input box
  ['⏎', '\r'],
];

const FONT_KEY = 'weft.fontSize';
function savedFont() {
  try {
    const n = Number(localStorage.getItem(FONT_KEY));
    if (n >= 8 && n <= 24) return n;
  } catch {}
  return window.innerWidth < 500 ? 12 : 14;
}

let term = null;
let fit = null;
let sock = null;
let current = null; // { id, name }
let ended = false;
// Nothing of this session on the screen yet: the first connection asks for
// the recent output. A reconnect keeps what is on the screen and only has
// weft draw itself again.
let fresh = true;
let retry = 0;
let retryTimer = null;

function setState(text, cls) {
  const s = $('state');
  s.textContent = text;
  s.className = `state ${cls || ''}`;
}

function ensureTerm() {
  if (term) return;
  term = new Terminal({
    fontFamily: 'ui-monospace, Menlo, "SF Mono", monospace',
    fontSize: savedFont(),
    scrollback: 5000,
    cursorBlink: false,
    allowProposedApi: false,
    macOptionIsMeta: true,
    theme: {
      background: '#1B1D1A',
      foreground: '#EDE8DC',
      cursor: '#EDE8DC',
      selectionBackground: '#4A4F43',
    },
  });
  fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open($('term'));
  // xterm's default DOM renderer. Its canvas and WebGL renderers are faster,
  // but in Chrome's phone emulation both drew the screen at the wrong pixel
  // density (scaled up and cut off); not worth it until seen right on a phone.
  term.onData((d) => send(enc.encode(d)));
  term.onBinary((d) => send(Uint8Array.from(d, (c) => c.charCodeAt(0))));

  const keys = $('keys');
  for (const [label, seq] of KEYS) {
    const b = el('button', null, label);
    // Keep the phone's keyboard up: the button must not take the focus.
    b.addEventListener('pointerdown', (e) => e.preventDefault());
    b.addEventListener('click', () => {
      send(enc.encode(seq));
      term.focus();
    });
    keys.append(b);
  }
  $('smaller').addEventListener('click', () => zoom(-1));
  $('bigger').addEventListener('click', () => zoom(1));
  $('back').addEventListener('click', () => {
    location.hash = '';
  });

  let t = null;
  const relayout = () => {
    const vv = window.visualViewport;
    document.documentElement.style.setProperty('--vh', `${vv ? vv.height : window.innerHeight}px`);
    clearTimeout(t);
    t = setTimeout(refit, 120);
  };
  window.addEventListener('resize', relayout);
  window.visualViewport?.addEventListener('resize', relayout);
  relayout();
}

function zoom(by) {
  const size = Math.min(24, Math.max(8, term.options.fontSize + by));
  term.options.fontSize = size;
  try { localStorage.setItem(FONT_KEY, String(size)); } catch {}
  refit(true);
}

// Every new size makes weft clear the screen and draw it again, so the size
// changes as little as it can. The width follows the screen. The height only
// ever shrinks (the keyboard coming up), until the width or the text size
// changes: the keyboard going down and up again leaves the size alone.
function refit(anew = false) {
  if (!term || $('session').hidden) return;
  const want = fit.proposeDimensions();
  if (!want || !want.cols || !want.rows) return;
  const cols = want.cols;
  const rows = anew || cols !== term.cols ? want.rows : Math.min(term.rows, want.rows);
  if (cols === term.cols && rows === term.rows) return;
  term.resize(cols, rows);
  if (sock && sock.readyState === WebSocket.OPEN) {
    sock.send(JSON.stringify({ resize: [cols, rows] }));
  }
}

function send(bytes) {
  if (sock && sock.readyState === WebSocket.OPEN) sock.send(bytes);
}

function openSession(target, name) {
  ensureTerm();
  document.body.classList.add('in-session');
  $('home').hidden = true;
  $('session').hidden = false;
  $('title').textContent = name || '';
  document.title = `weft · ${name || ''}`;
  current = { ...target, name };
  fresh = true;
  ended = false;
  retry = 0;
  connect();
}

function connect() {
  clearTimeout(retryTimer);
  if (sock) {
    sock.onclose = null;
    sock.close();
  }
  if (fresh) refit(true);
  setState('connecting…');
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  sock = ws;

  ws.onopen = () => {
    const hello = { cols: term.cols, rows: term.rows, replay: fresh };
    if (current.id) hello.id = current.id;
    else hello.folder = current.folder;
    ws.send(JSON.stringify(hello));
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data !== 'string') {
      term.write(new Uint8Array(ev.data));
      return;
    }
    const msg = JSON.parse(ev.data);
    if (msg.session) {
      // From here on, coming back means this session, not "the folder's".
      current = { id: msg.session.id, name: msg.session.name };
      if (location.hash !== `#${msg.session.id}`) history.replaceState(null, '', `#${msg.session.id}`);
      $('title').textContent = msg.session.name;
      document.title = `weft · ${msg.session.name}`;
      if (fresh) term.reset();
      fresh = false;
      retry = 0;
      setState('live', 'live');
      term.focus();
    } else if (msg.error) {
      ended = true;
      setState(msg.error, 'bad');
    } else if (msg.exit != null) {
      ended = true;
      setState(`ended (${msg.exit})`);
      term.write('\r\n\x1b[2m[the session has ended]\x1b[0m\r\n');
    }
  };
  ws.onclose = () => {
    if (sock !== ws || ended || $('session').hidden) return;
    const wait = Math.min(5000, 500 * 2 ** retry++);
    setState('reconnecting…', 'bad');
    retryTimer = setTimeout(connect, wait);
  };
}

function leaveSession() {
  clearTimeout(retryTimer);
  if (sock) {
    sock.onclose = null;
    sock.close();
    sock = null;
  }
  current = null;
}

// A phone that slept comes back to a closed socket: reconnect at once.
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible' || !current || ended) return;
  if (!sock || sock.readyState > WebSocket.OPEN) {
    retry = 0;
    connect();
  }
});

function route() {
  const id = location.hash.slice(1);
  if (/^[0-9a-f]{32}$/.test(id)) {
    if (!current || current.id !== id) openSession({ id }, '');
  } else {
    leaveSession();
    showHome();
  }
}
window.addEventListener('hashchange', route);
route();

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('sw.js').catch(() => {});
}
