// A hosted copy of the pages for the browser tests: crates/web/static served as plain
// files from another origin, the way a static host would serve them, with nothing of
// amem's behind it. Playwright starts it (playwright.config.ts); the page is opened as
// http://localhost:<port>/, an origin other than the server's.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../../crates/web/static/', import.meta.url));
const port = Number(process.env.HOSTED_PORT ?? 4311);
const types = {
  '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.md': 'text/markdown',
};

const serve = async (req, res) => {
  const path = decodeURIComponent(new URL(req.url ?? '/', 'http://x').pathname);
  const file = normalize(join(root, path.endsWith('/') ? `${path}index.html` : path));
  if (!file.startsWith(root.endsWith(sep) ? root : root + sep)) { res.writeHead(403).end(); return; }
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' }).end(body);
  } catch {
    res.writeHead(404).end('not found');
  }
};
// both loopback addresses, since the browser may take either for localhost
createServer(serve).listen(port, '127.0.0.1');
createServer(serve).on('error', () => { /* no IPv6 here */ }).listen(port, '::1');
