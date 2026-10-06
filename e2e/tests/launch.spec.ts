// The launch form and the session it starts: the form offers the harnesses with none
// chosen, previews the command line it will run, and a started session is a terminal
// that streams, takes input, and is listed on the front page.
import { test, expect } from './support/fixtures';

test('there is no default harness, and the preview follows the form', async ({ signedIn: page }) => {
  const start = page.getByRole('button', { name: 'Start' });
  await expect(start).toBeDisabled();
  await expect(page.getByText('there is no default harness')).toBeVisible();

  await page.locator('#harness').selectOption('codex');
  await expect(start).toBeEnabled();
  await page.locator('#auth').selectOption('key');
  await page.locator('#provider').selectOption('openai');
  await page.locator('#model').fill('gpt-test');
  await expect(page.locator('#cmd')).toHaveText('amem launch --harness codex --auth key --provider openai --model gpt-test');

  // a login is the harness's own: no destination to pick
  await page.locator('#auth').selectOption('subscription');
  await expect(page.locator('#provider')).toBeDisabled();

  // task mode takes no prompt
  await page.getByLabel('Work the next task from the ledger').check();
  await expect(page.getByLabel(/Prompt/)).toBeDisabled();
  await expect(page.locator('#cmd')).toContainText('amem work --harness codex');
});

test('a key with no credential behind it lists no models, and says why', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.locator('#auth').selectOption('key');
  await expect(page.locator('#models-hint')).toContainText('no list');
});

test('a started session streams, echoes what is typed, and is listed', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByLabel(/Prompt/).fill('summarise the record');
  await page.getByRole('button', { name: 'Start' }).click();
  await expect(page).toHaveURL(/\/s\/[0-9a-f]{32}$/);
  await expect(page.locator('#status')).toHaveText('connected');
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  await expect(page.locator('#title')).toHaveText('claude: summarise the record');

  await page.locator('.xterm').click();
  await page.keyboard.type('hello from the browser');
  await page.keyboard.press('Enter');
  await expect(page.locator('.xterm-rows')).toContainText('hello from the browser');

  // a reload lands in the same place: the scrollback is the server's
  await page.reload();
  await expect(page.locator('.xterm-rows')).toContainText('hello from the browser');

  const id = new URL(page.url()).pathname.split('/').pop();
  await page.goto('/');
  const row = page.locator(`a[href="/s/${id}"]`);
  await expect(row).toHaveText('claude: summarise the record');
  await expect(page.locator('li', { has: row })).toContainText('running');
});

test('a killed session says it exited', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const id = new URL(page.url()).pathname.split('/').pop();
  // the page's own key, through the page's own client
  const status = await page.evaluate(
    async (sid) => (await (window as unknown as { amem: { fetch: (p: string, o: object) => Promise<Response> } })
      .amem.fetch(`/api/sessions/${sid}`, { method: 'DELETE' })).status,
    id,
  );
  expect(status).toBe(204);
  await expect(page.locator('#status')).toHaveText(/^exited/);
});
