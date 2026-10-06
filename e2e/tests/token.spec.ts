// The key in front of the server, as a browser meets it: a pairing link is traded once
// for a key the page keeps in its origin's storage, the code leaves the address bar and
// is good once, no cookie is set, a page without a key says what to do, and the API
// answers nothing without one.
import { test, expect, pairingCode, storedKey, token } from './support/fixtures';

test('a pairing link gives the page a key of its own and leaves no trace in the address', async ({ page, context, request }) => {
  const code = await pairingCode(request);
  await page.goto(`/?pair=${code}`);
  await expect.poll(() => storedKey(page)).not.toBeNull();
  expect(new URL(page.url()).search).toBe('');
  expect(await storedKey(page)).not.toBe(token());
  expect(await context.cookies(), 'a cookie would go to every port on the host').toEqual([]);
  await expect(page.locator('#harness')).toContainText('claude');
  // a reload, or a return later, needs no link
  await page.reload();
  await expect(page.locator('#harness')).toContainText('claude');
});

test('a pairing link is good once', async ({ browser, request }) => {
  const code = await pairingCode(request);
  const first = await browser.newPage();
  await first.goto(`/?pair=${code}`);
  await expect.poll(() => storedKey(first)).not.toBeNull();
  const second = await browser.newPage();
  await second.goto(`/?pair=${code}`);
  await expect(second.locator('#form-hint')).toContainText('used already');
  expect(await storedKey(second)).toBeNull();
  await first.close();
  await second.close();
});

test('a page that is not paired says how to pair it', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText('This browser is not paired with this server')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Start' })).toBeDisabled();
});

test('the API wants a key, and a page from another origin is refused even with one', async ({ request }) => {
  expect((await request.get('/api/sessions')).status()).toBe(401);
  const auth = { Authorization: `Bearer ${token()}` };
  expect((await request.get('/api/sessions', { headers: auth })).status()).toBe(200);
  const foreign = await request.get('/api/sessions', { headers: { ...auth, Origin: 'https://attacker.example' } });
  expect(foreign.status()).toBe(403);
});
