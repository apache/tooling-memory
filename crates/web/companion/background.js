// The companion's worker: one websocket to the amem server the page paired it with, and
// a fetch for each ask that comes down it, made with this browser's cookies and only to
// a host the ask names. The hosts are the grant's at the session's label, as the memory
// server computed them; the memory server decided the URL by its policy before asking,
// and decides the URL that answered before it keeps anything. What goes back is the
// status, the content type, the URL that answered, and the body as text, cut at the
// ask's cap. Nothing is kept here: no page, no key on disk; the pairing lives in session
// storage, which goes with the browser.

const ACCEPT = 'text/html, text/plain;q=0.9, application/json;q=0.8, */*;q=0.1';
const FETCH_TIMEOUT_MS = 45000;

let linked = null; // { server, key }, from the page
let socket = null;
console.info('amem companion: worker started');

// the content script reaches an amem page by the manifest when the page loads after the
// extension; a page open already, when the extension is installed or the browser starts,
// and a page that loads while the extension is still starting, get it from here
const PAGES = ['http://127.0.0.1/*', 'http://localhost/*'];
function isPage(url) {
  try { const u = new URL(url); return u.protocol === 'http:' && ['127.0.0.1', 'localhost'].includes(u.hostname); } catch (_) { return false; }
}
function reach(tabId) {
  chrome.scripting.executeScript({ target: { tabId }, files: ['content.js'] })
    .catch((e) => console.warn(`amem companion: could not reach the page in tab ${tabId}: ${e && e.message || e}`));
}
async function reachOpenPages() {
  let tabs = [];
  try { tabs = await chrome.tabs.query({ url: PAGES }); } catch (e) { console.warn(`amem companion: tabs: ${e}`); return; }
  for (const t of tabs) if (t.id !== undefined) reach(t.id);
}
chrome.runtime.onInstalled.addListener(reachOpenPages);
chrome.runtime.onStartup.addListener(reachOpenPages);
chrome.tabs.onUpdated.addListener((tabId, change, tab) => {
  if (change.status === 'complete' && tab && isPage(tab.url || '')) reach(tabId);
});

// `*.example.org` names hosts under it, `host` or `host:port` exactly; as the gate has it
function hostMatches(pattern, host) {
  const p = String(pattern).toLowerCase();
  host = host.toLowerCase();
  return p.startsWith('*.') ? host.endsWith(p.slice(1)) : p === host;
}

// whether a URL is one the ask lets this browser go to: http or https, on a named host
function allowed(url, hosts) {
  let u;
  try { u = new URL(url); } catch (_) { return false; }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
  const withPort = u.port ? `${u.hostname}:${u.port}` : u.hostname;
  return (hosts || []).some((h) => hostMatches(h, u.hostname) || hostMatches(h, withPort));
}

async function readCapped(response, cap) {
  if (!response.body) return { bytes: new Uint8Array(0), truncated: false };
  const reader = response.body.getReader();
  const chunks = [];
  let n = 0;
  let truncated = false;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    if (n + value.length > cap) {
      chunks.push(value.subarray(0, cap - n));
      n = cap;
      truncated = true;
      await reader.cancel().catch(() => {});
      break;
    }
    chunks.push(value);
    n += value.length;
  }
  const bytes = new Uint8Array(n);
  let at = 0;
  for (const c of chunks) { bytes.set(c, at); at += c.length; }
  return { bytes, truncated };
}

function decoderFor(contentType) {
  const m = /charset=["']?([^;"'\s]+)/i.exec(contentType || '');
  try { return new TextDecoder(m ? m[1] : 'utf-8'); } catch (_) { return new TextDecoder('utf-8'); }
}

// one ask: the fetch, with cookies, and what came back
async function fetchFor(ask) {
  const hosts = ask.hosts || [];
  if (!allowed(ask.url, hosts)) return { error: `${ask.url} is not on a host this session may reach` };
  const control = new AbortController();
  const timer = setTimeout(() => control.abort(), FETCH_TIMEOUT_MS);
  try {
    const r = await fetch(ask.url, {
      method: 'GET', credentials: 'include', redirect: 'follow', cache: 'no-store',
      headers: { accept: ACCEPT }, signal: control.signal,
    });
    // a redirect off the hosts is not read: the answer names where it went, and no body
    if (!allowed(r.url, hosts)) {
      await r.body?.cancel().catch(() => {});
      return { error: `${ask.url} answered from ${r.url}, which this session may not reach`, final_url: r.url };
    }
    const cap = ask.max_bytes || 2000000;
    const { bytes, truncated } = await readCapped(r, cap);
    const contentType = r.headers.get('content-type') || '';
    return {
      status: r.status, content_type: contentType, final_url: r.url,
      body: decoderFor(contentType).decode(bytes), truncated,
    };
  } catch (e) {
    return { error: `${ask.url}: ${(e && e.message) || e}` };
  } finally {
    clearTimeout(timer);
  }
}

function connect() {
  if (!linked || socket) return;
  let u;
  try { u = new URL(linked.server); } catch (_) { linked = null; return; }
  if (u.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(u.hostname)) { linked = null; return; }
  const ws = new WebSocket(`ws://${u.host}/ws/companion`, ['amem', `amem.key.${linked.key}`]);
  socket = ws;
  ws.onopen = () => {
    console.info(`amem companion: connected to ${linked.server}`);
    ws.send(JSON.stringify({ type: 'hello', version: chrome.runtime.getManifest().version }));
  };
  ws.onmessage = async (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch (_) { return; }
    if (m.type === 'ping') { ws.send(JSON.stringify({ type: 'pong' })); return; }
    if (m.type !== 'fetch' || m.id === undefined) return;
    const got = await fetchFor(m);
    got.id = m.id;
    if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(got));
  };
  ws.onclose = (ev) => {
    console.warn(`amem companion: socket closed (${ev.code}); the alarm reconnects`);
    if (socket === ws) socket = null;
  };
  ws.onerror = () => { /* onclose follows */ };
}

// the page hands over its server and key; a new pair replaces the old socket
chrome.runtime.onMessage.addListener((m) => {
  if (!m || m.type !== 'pair' || typeof m.server !== 'string' || typeof m.key !== 'string') return;
  console.info(`amem companion: paired with ${m.server}`);
  if (!linked || linked.server !== m.server || linked.key !== m.key) {
    linked = { server: m.server, key: m.key };
    chrome.storage.session.set({ linked }).catch(() => {});
    if (socket) { const s = socket; socket = null; s.close(); }
  }
  connect();
});

// the worker may be stopped and started again: the pairing comes back from session
// storage, and the alarm reconnects a socket that dropped
async function restore() {
  if (linked) return;
  try {
    const v = await chrome.storage.session.get('linked');
    if (v && v.linked) linked = v.linked;
  } catch (_) { /* no storage */ }
}
chrome.alarms.create('amem-reconnect', { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(async () => { await restore(); connect(); });
restore().then(connect);
