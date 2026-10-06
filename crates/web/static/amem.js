// The page's key to this server. A pairing link (?pair=<code>) is traded once for a key
// of this browser's own, kept in this origin's storage, which unlike a cookie is not
// shared with other ports on the host. Calls to this origin carry it as a bearer; calls
// elsewhere, a bundle on another host say, carry nothing. A websocket offers it as a
// subprotocol, since a browser lets it carry no header.
(function () {
  const STORE = 'amem-key';
  const read = () => { try { return localStorage.getItem(STORE); } catch (_) { return null; } };
  const write = (k) => { try { localStorage.setItem(STORE, k); } catch (_) { /* private mode */ } };
  const amem = { pairError: null };
  window.amem = amem;

  async function pairFromUrl() {
    const u = new URL(location.href);
    const code = u.searchParams.get('pair');
    if (!code) return;
    // off the address bar and out of history before anything else
    u.searchParams.delete('pair');
    history.replaceState(null, '', u.pathname + u.search + u.hash);
    const r = await fetch('/api/pair/redeem', {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ code }),
    });
    if (r.ok) write((await r.json()).key);
    else amem.pairError = await r.text();
  }

  amem.ready = pairFromUrl().catch((e) => { amem.pairError = String(e); });
  amem.paired = () => !!read();
  amem.fetch = async (path, opts = {}) => {
    await amem.ready;
    const headers = new Headers(opts.headers || {});
    const k = read();
    if (k && new URL(path, location.href).origin === location.origin) headers.set('authorization', `Bearer ${k}`);
    return fetch(path, { ...opts, headers });
  };
  amem.socket = (path) => {
    const k = read();
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    return new WebSocket(`${scheme}://${location.host}${path}`, k ? ['amem', `amem.key.${k}`] : ['amem']);
  };
  // what a page says when it has no key, or the server no longer knows it
  amem.unpaired = () => amem.pairError
    || 'This browser is not paired with this server. Run amem, which opens a link that pairs it.';
})();
