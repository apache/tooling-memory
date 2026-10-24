// The companion's worker: one websocket to the amem server the page paired it with, and
// a fetch for each ask that comes down it, made with this browser's cookies and only to
// a host the ask names. The hosts are the grant's at the session's label, as the memory
// server computed them; the memory server decided the URL by its policy before asking,
// and decides the URL that answered before it keeps anything. What goes back is the
// status, the content type, the URL that answered, and the body as text, cut at the
// ask's cap. Nothing is kept here: no page, no key on disk; the pairing lives in session
// storage, which goes with the browser.
//
// The hosts hold on the way, not only at the ends. While an ask is in flight, session
// rules of declarativeNetRequest block any request of this extension's own to a host
// the ask does not name, so a redirect elsewhere is stopped before it is made, and the
// answer says where it was going. The rules name this extension as the initiator and
// touch nothing else in the browser: no tab of the owner's, no other extension.
//
// An ask that says `render` is opened in a tab of this browser instead of fetched: the
// page runs as it would for the owner, and what goes back is the document as the page
// built it. Navigation of that tab, the top frame and any frame in it, is held to the
// hosts the same way; what the page itself loads, scripts, images, is the page's own, as
// it is when the owner opens it. The tab is closed when the answer is read.

const ACCEPT = 'text/html, text/plain;q=0.9, application/json;q=0.8, */*;q=0.1';
const FETCH_TIMEOUT_MS = 45000;
// A rendered page is read once its scripts have had their turn: a tab the owner is not
// looking at runs timers on whole seconds, so the first read waits one out, and the page
// is read again until two reads agree or the wait is up.
const SETTLE_MS = 1200;
const QUIET_MS = 500;
const SETTLE_MAX_MS = 6000;

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

// The hosts as rules. One block rule for everything this extension asks for, and an
// allow rule above it per host an in-flight ask names; `requestDomains` cannot say a
// port or keep the apex out of `*.example.org`, so each host is a regular expression
// over the URL, as `allowed` reads it. A tab that renders has its own pair, on the tab
// id and the frames' navigation. Rules are rebuilt from the asks in flight, so none
// outlive their ask, and a worker that starts with none in flight clears what a worker
// that died left behind.
const inflight = new Map(); // ask -> { hosts, tabId? }
let nextAsk = 0;
const NAV = ['main_frame', 'sub_frame'];
const escapeRe = (s) => s.replace(/[.*+?^${}()|[\]\\\/]/g, '\\$&');
function hostRegex(pattern) {
  const p = String(pattern).toLowerCase();
  if (p.startsWith('*.')) return `^https?://[^/?#:@]*${escapeRe(p.slice(1))}(:\\d+)?/`;
  const m = /^(.*):(\d+)$/.exec(p); // host:port, `[::1]:8080` included; a bare `[::1]` ends in `]`
  if (m) return `^https?://${escapeRe(m[1])}:${m[2]}/`;
  return `^https?://${escapeRe(p)}(:\\d+)?/`;
}
function rulesNow() {
  const me = chrome.runtime.id;
  const rules = [];
  let id = 1;
  const fetchHosts = new Set();
  let fetching = false;
  for (const a of inflight.values()) {
    if (a.tabId === undefined) { fetching = true; for (const h of a.hosts) fetchHosts.add(h); }
  }
  if (fetching) {
    const own = { initiatorDomains: [me], resourceTypes: ['xmlhttprequest'] };
    rules.push({ id: id++, priority: 1, action: { type: 'block' }, condition: own });
    for (const h of fetchHosts) {
      rules.push({ id: id++, priority: 2, action: { type: 'allow' }, condition: { ...own, regexFilter: hostRegex(h), isUrlFilterCaseSensitive: false } });
    }
  }
  for (const a of inflight.values()) {
    if (a.tabId === undefined) continue;
    const tab = { tabIds: [a.tabId], resourceTypes: NAV };
    rules.push({ id: id++, priority: 1, action: { type: 'block' }, condition: tab });
    for (const h of a.hosts) {
      rules.push({ id: id++, priority: 2, action: { type: 'allow' }, condition: { ...tab, regexFilter: hostRegex(h), isUrlFilterCaseSensitive: false } });
    }
  }
  return rules;
}
let syncing = Promise.resolve();
function sync() {
  const step = async () => {
    const have = await chrome.declarativeNetRequest.getSessionRules();
    await chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: have.map((r) => r.id), addRules: rulesNow() });
  };
  syncing = syncing.then(step, step);
  return syncing;
}
sync().catch((e) => console.warn(`amem companion: rules: ${e && e.message || e}`));

// What the network saw of this extension's own requests, and of the tabs it renders:
// where a redirect was going, and why a request did not go, so a refusal can say so.
// The events land after the fetch has failed; the trace is read when it has.
const own = () => `chrome-extension://${chrome.runtime.id}`;
const traces = new Map(); // requestId -> { url, hops: [], error }
const frames = new Map(); // tabId -> { status, error }
const forget = (map, keep) => { while (map.size > keep) map.delete(map.keys().next().value); };
chrome.webRequest.onBeforeRequest.addListener((d) => {
  if (d.tabId === -1 && d.initiator === own() && !traces.has(d.requestId)) { traces.set(d.requestId, { url: d.url, hops: [] }); forget(traces, 64); }
}, { urls: ['<all_urls>'] });
chrome.webRequest.onBeforeRedirect.addListener((d) => {
  const t = traces.get(d.requestId);
  if (t) t.hops.push(d.redirectUrl);
}, { urls: ['<all_urls>'] });
chrome.webRequest.onErrorOccurred.addListener((d) => {
  const t = traces.get(d.requestId);
  if (t) t.error = d.error;
  if (d.type === 'main_frame' && d.tabId !== -1) { frames.set(d.tabId, { url: d.url, error: d.error }); forget(frames, 64); }
}, { urls: ['<all_urls>'] });
chrome.webRequest.onCompleted.addListener((d) => {
  if (d.type === 'main_frame' && d.tabId !== -1) { frames.set(d.tabId, { url: d.url, status: d.statusCode }); forget(frames, 64); }
}, { urls: ['<all_urls>'] });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function traceOf(url) {
  for (let i = 0; i < 20; i++) {
    const found = [...traces.values()].reverse().find((t) => t.url === url);
    if (found && found.error) return found;
    await sleep(50);
  }
  return [...traces.values()].reverse().find((t) => t.url === url);
}

function cut(bytes, cap) {
  return bytes.length > cap ? { bytes: bytes.subarray(0, cap), truncated: true } : { bytes, truncated: false };
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

const offHosts = (url, to) => ({ error: `${url} was sent to ${to}, which this session may not reach: stopped before it was fetched`, final_url: to });

// one ask: the fetch, with cookies, under the rules, and what came back
async function fetchFor(ask) {
  const hosts = ask.hosts || [];
  if (!allowed(ask.url, hosts)) return { error: `${ask.url} is not on a host this session may reach` };
  const id = ++nextAsk;
  inflight.set(id, { hosts });
  const control = new AbortController();
  const timer = setTimeout(() => control.abort(), FETCH_TIMEOUT_MS);
  try {
    await sync();
    const r = await fetch(ask.url, {
      method: 'GET', credentials: 'include', redirect: 'follow', cache: 'no-store',
      headers: { accept: ACCEPT }, signal: control.signal,
    });
    // the rules stop a redirect off the hosts; this is the last word if they did not
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
    const t = await traceOf(ask.url);
    if (t && t.hops.length && /BLOCKED_BY_CLIENT/.test(t.error || '')) return offHosts(ask.url, t.hops[t.hops.length - 1]);
    return { error: `${ask.url}: ${(e && e.message) || e}` };
  } finally {
    clearTimeout(timer);
    inflight.delete(id);
    sync().catch(() => {});
  }
}

// a tab as it was opened to render in: it shows its own about:blank, and the navigation
// started in it is pending
const opening = (t) => t.url === 'about:blank' && Boolean(t.pendingUrl);

// the tabs rendered in, each with whether the navigation started there committed a
// document: the tab took a URL other than about:blank
const rendering = new Map(); // tabId -> { committed }
chrome.tabs.onUpdated.addListener((tabId, change) => {
  const r = rendering.get(tabId);
  if (r && change.url && change.url !== 'about:blank') r.committed = true;
});

// why a tab that shows a URL off the hosts is refused. On about:blank with no document of
// the navigation committed, the page was fetched and answered with no document, a 204 or
// a download; a commit reported late gets one quiet spell to land. Anywhere else, the
// page was sent there.
async function offTab(ask, tabId, url) {
  const committed = () => (rendering.get(tabId) || {}).committed;
  if (url === 'about:blank' && !committed()) {
    await sleep(QUIET_MS);
    if (!committed()) return { error: `${ask.url} produced no page to read (no document was committed)` };
  }
  return offHosts(ask.url, url);
}

// one ask to render: a tab of this browser, held to the hosts, read when it is built
async function renderFor(ask) {
  const hosts = ask.hosts || [];
  if (!allowed(ask.url, hosts)) return { error: `${ask.url} is not on a host this session may reach` };
  let tab;
  try {
    tab = await chrome.tabs.create({ url: 'about:blank', active: false });
  } catch (_) {
    const w = await chrome.windows.create({ url: 'about:blank', focused: false }).catch(() => null);
    tab = w && w.tabs && w.tabs[0];
  }
  if (!tab || tab.id === undefined) return { error: `${ask.url}: this browser would open no tab to render it in` };
  const id = ++nextAsk;
  inflight.set(id, { hosts, tabId: tab.id });
  frames.delete(tab.id);
  try {
    await sync();
    const deadline = Date.now() + FETCH_TIMEOUT_MS;
    let t;
    // the navigation starts once the tab's own about:blank has committed: a commit of it
    // while the navigation is on its way takes the pending URL off the tab, and the tab
    // then shows about:blank with nothing pending, as it would if the page had gone there
    for (;;) {
      t = await chrome.tabs.get(tab.id);
      if (t.url === 'about:blank' && t.status === 'complete') break;
      if (Date.now() > deadline) return { error: `${ask.url}: the tab to render it in did not open in ${FETCH_TIMEOUT_MS / 1000} seconds` };
      await sleep(50);
    }
    rendering.set(tab.id, { committed: false });
    await chrome.tabs.update(tab.id, { url: ask.url });
    // until the page says it is complete; a navigation the rules stopped, or one the
    // network refused, is the answer as soon as the frame reports it, whatever the tab says.
    // Until the navigation started here commits, the tab shows its own about:blank with
    // that navigation pending: not a place the page was sent to, so it is not judged.
    // From the first look that shows anything else, every URL the tab shows is judged,
    // about:blank with them.
    let shown = false;
    for (;;) {
      await sleep(100);
      t = await chrome.tabs.get(tab.id);
      const frame = frames.get(tab.id);
      if (frame && frame.error) {
        if (!allowed(frame.url, hosts)) return offHosts(ask.url, frame.url);
        return { error: `${ask.url}: the browser could not load it: ${frame.error}`, final_url: frame.url };
      }
      shown = shown || !opening(t);
      if (shown && !allowed(t.url || '', hosts)) return offTab(ask, tab.id, t.url || '');
      if (t.status === 'complete' && t.url && t.url !== 'about:blank') break;
      if (Date.now() > deadline) return { error: `${ask.url}: the page did not finish loading in ${FETCH_TIMEOUT_MS / 1000} seconds` };
    }
    await sleep(SETTLE_MS);
    t = await chrome.tabs.get(tab.id);
    if (!allowed(t.url || '', hosts)) return offHosts(ask.url, t.url || '');
    const frame = frames.get(tab.id) || {};
    if (frame.error) {
      if (!allowed(frame.url, hosts)) return offHosts(ask.url, frame.url);
      return { error: `${ask.url}: the browser could not load it: ${frame.error}`, final_url: frame.url };
    }
    const read = async () => {
      const [r] = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: () => ({ url: location.href, type: document.contentType, html: document.documentElement ? document.documentElement.outerHTML : '' }),
      });
      return r && r.result;
    };
    let got;
    try {
      got = await read();
      for (let waited = SETTLE_MS; waited < SETTLE_MAX_MS; waited += QUIET_MS) {
        await sleep(QUIET_MS);
        const again = await read();
        if (got && again && again.html === got.html && again.url === got.url) break;
        got = again;
      }
    } catch (e) {
      // the page went somewhere while it was read: an error page, if the rules stopped it
      t = await chrome.tabs.get(tab.id);
      if (!allowed(t.url || '', hosts)) return offHosts(ask.url, t.url || '');
      throw e;
    }
    if (!got) return { error: `${ask.url}: the rendered page could not be read` };
    if (!allowed(got.url, hosts)) return offHosts(ask.url, got.url);
    const cap = ask.max_bytes || 2000000;
    const { bytes, truncated } = cut(new TextEncoder().encode(got.html), cap);
    return {
      status: frame.status || 200, content_type: `${got.type || 'text/html'}; charset=utf-8`, final_url: got.url,
      body: new TextDecoder('utf-8').decode(bytes), truncated, rendered: true,
    };
  } catch (e) {
    return { error: `${ask.url}: ${(e && e.message) || e}` };
  } finally {
    chrome.tabs.remove(tab.id).catch(() => {});
    rendering.delete(tab.id);
    inflight.delete(id);
    sync().catch(() => {});
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
    const got = await (m.render ? renderFor(m) : fetchFor(m));
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
