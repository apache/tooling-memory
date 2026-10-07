// Every spec imports `test` and `expect` from here, not from @playwright/test: the page
// fixture fails a test on an uncaught error in the page, `signedIn` is a page paired the
// way `amem` pairs one, and `token` and `ids` are the server's, read from what serve.sh
// left behind. `hostedURL` is the hosted copy of the pages, on its own origin.
import { test as base, expect, type APIRequestContext, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const state = join(__dirname, '..', '..', '.state');

/** The token the server started with, from its state file. */
export function token(): string {
  const web = JSON.parse(readFileSync(join(state, 'home/memory/state/web.json'), 'utf8')) as { token: string };
  return web.token;
}

/** The seeded entries' ids, in the order serve.sh wrote them. */
export function ids(): string[] {
  return readFileSync(join(state, 'ids'), 'utf8').split('\n').filter(Boolean);
}

/** The record's root, for a test that writes to it the way a session would. */
export const root = join(state, 'home/memory');

/** A pairing code, minted with the token as `amem` mints one. */
export async function pairingCode(request: APIRequestContext): Promise<string> {
  const r = await request.post('/api/pair', { headers: { Authorization: `Bearer ${token()}` } });
  expect(r.status()).toBe(200);
  return ((await r.json()) as { code: string }).code;
}

/** What the server says of the session the page shows, through the page's own key. */
export async function sessionInfo(page: Page): Promise<{ launch: Record<string, string | null> }> {
  const id = new URL(page.url()).searchParams.get('id');
  return page.evaluate(async (sid) => {
    const amem = (window as unknown as { amem: { fetch: (p: string) => Promise<Response> } }).amem;
    return (await amem.fetch(`/api/sessions/${sid}`)).json();
  }, id);
}

/** A session started through the API with the token, for a test that needs one to look at. */
export async function startSession(request: APIRequestContext, body: object = { harness: 'claude' }): Promise<string> {
  const r = await request.post('/api/sessions', { headers: { Authorization: `Bearer ${token()}` }, data: body });
  expect(r.status()).toBe(200);
  return ((await r.json()) as { id: string }).id;
}

/** The key the page holds, if it is paired. */
export function storedKey(page: Page): Promise<string | null> {
  return page.evaluate(() => localStorage.getItem('amem-key'));
}

type Fixtures = {
  /** A page that has been through a pairing link, so it holds a key of its own. */
  signedIn: Page;
};

export const test = base.extend<Fixtures>({
  page: async ({ page }, use) => {
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await use(page);
    expect(errors, 'uncaught errors in the page').toEqual([]);
  },
  signedIn: async ({ page, request }, use) => {
    await page.goto(`/?pair=${await pairingCode(request)}`);
    await expect.poll(() => storedKey(page)).not.toBeNull();
    await use(page);
  },
});

export { expect };

/** The hosted copy of the pages: crates/web/static as plain files on another origin. */
export const hostedURL = `http://localhost:${process.env.HOSTED_PORT ?? 4311}`;

/** The server, as a hosted page is told it in the pairing link. */
export const serverURL = `http://127.0.0.1:${process.env.PORT ?? 4310}`;

/** Where screenshots go: evidence for a pull request, ignored by git. */
export function shot(name: string): string {
  return join(__dirname, '..', '..', 'screenshots', `${name}.png`);
}
