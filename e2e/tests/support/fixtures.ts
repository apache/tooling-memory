// Every spec imports `test` and `expect` from here, not from @playwright/test: the page
// fixture fails a test on an uncaught error in the page, `signedIn` is a page paired the
// way `amem` pairs one, and `token` and `ids` are the server's, read from what serve.sh
// left behind.
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

/** Where screenshots go: evidence for a pull request, ignored by git. */
export function shot(name: string): string {
  return join(__dirname, '..', '..', 'screenshots', `${name}.png`);
}
