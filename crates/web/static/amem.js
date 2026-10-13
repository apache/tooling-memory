// The page's way to its server. Served by amem, the server is the page's own origin. A
// hosted copy of the pages, on another origin, learns its server from the pairing link
// amem opens, in the fragment, which a browser never sends to the host:
//   https://<host>/<path>/#server=http://127.0.0.1:4200&pair=<code>
// A pairing code is traded once for a key of this browser's own, kept in this origin's
// storage beside the server it belongs to; unlike a cookie, that storage is not shared
// with other ports on the same host. Calls to the server carry the key as a bearer and
// calls anywhere else carry nothing; a websocket offers it as a subprotocol, since a
// browser lets it carry no header.
(function () {
  const STORE = 'amem-key';
  const SERVER = 'amem-server';
  const LOOPBACK = ['127.0.0.1', 'localhost', '[::1]'];
  const get = (k) => { try { return localStorage.getItem(k); } catch (_) { return null; } };
  const set = (k, v) => { try { localStorage.setItem(k, v); } catch (_) { /* private mode */ } };
  const drop = (k) => { try { localStorage.removeItem(k); } catch (_) { /* private mode */ } };
  const amem = { pairError: null };
  window.amem = amem;

  // a server is an http origin on this machine, and nothing else
  function loopback(s) {
    try {
      const u = new URL(s);
      return u.protocol === 'http:' && LOOPBACK.includes(u.hostname) ? u.origin : null;
    } catch (_) { return null; }
  }

  // a copy is a page that has been told a server other than its own origin, by the link
  // it was opened on or one before it; served by amem, the page's origin is its server
  const opened = new URLSearchParams(location.hash.slice(1));
  const told = opened.get('server');
  const named = told ? loopback(told) : null;
  const stored = get(SERVER);
  amem.hosted = !!((named && named !== location.origin) || (stored && stored !== location.origin));
  amem.server = () => (amem.hosted ? get(SERVER) : location.origin);

  async function pairFromAddress() {
    const u = new URL(location.href);
    const fragment = new URLSearchParams(u.hash.slice(1));
    const code = u.searchParams.get('pair') || fragment.get('pair');
    if (told && !named) amem.pairError = 'That link names a server that is not on this machine.';
    // only a link that pairs moves the page to another server: one that only names a
    // server would otherwise unpair the page for anyone who can send it a link
    if (named && code && amem.hosted && named !== get(SERVER)) { set(SERVER, named); drop(STORE); }
    if (!code && !told) return;
    // off the address bar and out of history before anything else
    u.searchParams.delete('pair');
    fragment.delete('pair');
    fragment.delete('server');
    const rest = fragment.toString();
    history.replaceState(null, '', u.pathname + u.search + (rest ? `#${rest}` : ''));
    if (!code || !amem.server()) return;
    const r = await fetch(`${amem.server()}/api/pair/redeem`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ code }),
    });
    if (r.ok) set(STORE, (await r.json()).key);
    else amem.pairError = await r.text();
  }

  amem.ready = pairFromAddress().catch((e) => { amem.pairError = String(e); });
  amem.paired = () => !!(amem.server() && get(STORE));

  // the companion: an extension beside this page that fetches for a session with this
  // browser's own logins (crates/web/companion). Its content script runs in this window;
  // a paired page posts it the server and the key, and nothing else, to its own origin,
  // and says so again whenever the extension says hello. Nothing is posted unpaired.
  const COMPANION = 'amem-companion';
  amem.companion = null;
  function offerCompanion() {
    const k = get(STORE);
    const server = amem.server();
    if (!k || !server) return;
    window.postMessage({ type: `${COMPANION}-pair`, server, key: k }, location.origin);
  }
  window.addEventListener('message', (e) => {
    if (e.origin !== location.origin || !e.data || e.data.type !== `${COMPANION}-hello`) return;
    amem.companion = e.data.version || 'installed';
    offerCompanion();
  });
  amem.ready.then(offerCompanion);

  // `/api/...` is on the server; anything else is as written and carries no key
  amem.url = (path) => (path.startsWith('/') && amem.server() ? amem.server() + path : path);
  amem.fetch = async (path, opts = {}) => {
    await amem.ready;
    const url = amem.url(path);
    const headers = new Headers(opts.headers || {});
    const k = get(STORE);
    const server = amem.server();
    if (k && server && new URL(url, location.href).origin === server) headers.set('authorization', `Bearer ${k}`);
    return fetch(url, { ...opts, headers });
  };
  amem.socket = (path) => {
    const k = get(STORE);
    const base = new URL(amem.server() || location.origin);
    const scheme = base.protocol === 'https:' ? 'wss' : 'ws';
    return new WebSocket(`${scheme}://${base.host}${path}`, k ? ['amem', `amem.key.${k}`] : ['amem']);
  };
  // what a page says when it has no key, or the server no longer knows it
  amem.unpaired = () => amem.pairError
    || (amem.hosted && !amem.server()
      ? 'This page talks to amem on your own machine. Run amem there: it opens this page paired.'
      : 'This browser is not paired with this server. Run amem, which opens a link that pairs it.');
})();
