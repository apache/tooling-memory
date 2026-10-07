// A hosted copy of the pages: crates/web/static served as plain files from another
// origin, told its server in the pairing link's fragment. It pairs, lists the sessions,
// and starts one, every call going across origins to amem on this machine.
import { test, expect, hostedURL, pairingCode, serverURL, startSession } from './support/fixtures';

test('a hosted copy pairs from its link, lists the sessions and starts one', async ({ page, request }) => {
  const id = await startSession(request);
  const code = await pairingCode(request);
  await page.goto(`${hostedURL}/index.html#server=${encodeURIComponent(serverURL)}&pair=${code}`);
  await expect(page.locator('#pairing')).toContainText('Paired with');
  // the code and the server leave the address bar
  expect(new URL(page.url()).hash).toBe('');
  await expect(page.locator(`a[href="session.html?id=${id}"]`)).toBeVisible();

  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(new RegExp(`^${hostedURL}/session\\.html\\?id=[0-9a-f]{32}$`));
  await expect(page.locator('#status')).toHaveText('running');
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
});

test('a hosted copy that was never paired says to run amem', async ({ page }) => {
  await page.goto(`${hostedURL}/index.html`);
  await expect(page.locator('#unpaired')).toContainText('Run amem');
  await expect(page.getByRole('button', { name: 'Start session' })).toBeDisabled();
});

test('a hosted copy whose key is gone says amem runs on this machine, on every page', async ({ page, request }) => {
  const code = await pairingCode(request);
  await page.goto(`${hostedURL}/index.html#server=${encodeURIComponent(serverURL)}&pair=${code}`);
  await expect(page.locator('#pairing')).toContainText('Paired with');
  // the server it was told stays; the key does not
  await page.evaluate(() => localStorage.removeItem('amem-key'));
  for (const path of ['index.html', 'board.html']) {
    await page.goto(`${hostedURL}/${path}`);
    await expect(page.locator('#unpaired')).toContainText('on your own machine');
    await expect(page.locator('#pairing')).toContainText('Not paired');
  }
  await page.goto(`${hostedURL}/session.html?id=0123456789abcdef0123456789abcdef`);
  await expect(page.locator('#status')).toHaveText('not paired');
  await expect(page.locator('#notice')).toContainText('on your own machine');
});

test('the reader works as a static copy, and says what pairing would add', async ({ page }) => {
  await page.goto(`${hostedURL}/reader.html`);
  await expect(page.locator('#pairing')).toContainText('Not paired');
  await expect(page.locator('#domains-hint')).toContainText('A bundle URL or a folder works without it');
  await expect(page.getByRole('button', { name: 'Load' })).toBeEnabled();
});
