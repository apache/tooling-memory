// The companion's end on the amem page (crates/web/static/amem.js). The page, once it
// is paired, posts its server and its key to its own window; this script hands them to
// the extension's worker, which opens the socket to the server. Nothing else of the
// page is read, and nothing is posted to any other page: this script runs on loopback
// pages only, and reads only what its own origin posts to its own window.
(() => {
  // reached twice, by the manifest and by the worker: once is enough
  if (window.__amemCompanion) return;
  window.__amemCompanion = true;
  const T = 'amem-companion';
  window.addEventListener('message', (e) => {
    if (e.origin !== location.origin || !e.data || e.data.type !== `${T}-pair`) return;
    const { server, key } = e.data;
    if (typeof server !== 'string' || typeof key !== 'string') return;
    console.info(`amem companion: the page paired it with ${server}`);
    chrome.runtime.sendMessage({ type: 'pair', server, key }).catch((err) => console.warn(`amem companion: ${err}`));
  });
  // say hello, so a page that paired before this script ran offers its key again
  window.postMessage({ type: `${T}-hello`, version: chrome.runtime.getManifest().version }, location.origin);
})();
