// The launch form and the session it starts: the form offers the profiles and the
// harnesses with none chosen, previews the command line it will run, says what this host
// has for each harness, and a started session is a terminal that streams, takes input,
// and is listed on the front page.
import { test, expect, sessionInfo } from './support/fixtures';

test('there is no default harness, and the preview follows the form', async ({ signedIn: page }) => {
  const start = page.getByRole('button', { name: 'Start session' });
  await expect(start).toBeDisabled();
  await expect(page.getByText('Choose a profile or a harness first')).toBeVisible();

  await page.locator('#harness').selectOption('codex');
  await expect(start).toBeEnabled();
  await page.locator('#auth input[value="key"]').check();
  await page.locator('#provider').selectOption('openai');
  await page.locator('#model').fill('gpt-test');
  await expect(page.locator('#cmd')).toHaveText('amem launch --harness codex --auth key --provider openai --model gpt-test');

  // a login is the harness's own: no destination to pick
  await page.locator('#auth input[value="subscription"]').check();
  await expect(page.locator('#provider')).toBeDisabled();

  // task mode takes no prompt
  await page.getByLabel('Work the next task from the ledger').check();
  await expect(page.getByLabel('Prompt')).toBeDisabled();
  await expect(page.locator('#cmd')).toContainText('amem work --harness codex');
});

test('a key with no credential behind it lists no models, and says why', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.locator('#auth input[value="key"]').check();
  await expect(page.locator('#models-hint')).toContainText('No model list');
});

test('a profile fills the form, and the preview is the profile plus what was changed', async ({ signedIn: page }) => {
  await expect(page.locator('#profile option[value="codex"]')).toHaveCount(1);
  await page.locator('#profile').selectOption('codex');
  await expect(page.locator('#harness')).toHaveValue('codex');
  await expect(page.locator('#cmd')).toHaveText('amem codex');
  await expect(page.getByRole('button', { name: 'Start session' })).toBeEnabled();

  // the fields stay editable, and a change is a flag on top of the profile
  await page.locator('#model').fill('gpt-test');
  await expect(page.locator('#cmd')).toHaveText('amem codex --model gpt-test');
  await page.getByLabel('Work the next task from the ledger').check();
  await expect(page.locator('#cmd')).toHaveText('amem work codex --model gpt-test');

  // back to none: the whole command, spelled out
  await page.locator('#profile').selectOption('');
  await expect(page.locator('#cmd')).toContainText('amem work --harness codex');
});

test('the backend picked is the backend the session is started with', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.locator('#backend').selectOption('namespaces');
  await expect(page.locator('#cmd')).toContainText('--backend namespaces');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(/\/session\.html\?id=[0-9a-f]{32}$/);
  await expect(page.locator('#status')).toHaveText('running');
  const info = await sessionInfo(page);
  expect(info.launch.backend).toBe('namespaces');
  expect(info.launch.harness).toBe('claude');
});

test('each harness says what this host has for it, before anything starts', async ({ signedIn: page }) => {
  // the server under test has no harness installed and no login or key: the doctor's
  // words for what is missing, never a blank
  const claude = page.locator('#ready li[data-harness="claude"]');
  await expect(claude).toContainText(/not installed|no login/);
  await expect(page.locator('#ready li')).toHaveCount(10);

  await page.locator('#harness').selectOption('claude');
  await expect(claude).toHaveClass(/is-open/);
  await expect(page.locator('#auth-ready')).toContainText(/With subscription: (not installed|no login)/);
  // the auth choices carry the verdict in their names, not only in a glyph
  await expect(page.getByRole('radio', { name: /^Key, (not installed|no ANTHROPIC_API_KEY)$/ })).toHaveCount(1);
  await expect(page.locator('#form-hint')).toContainText('may stop at once');

  // asking the gateway is a separate, slower question
  await page.getByRole('button', { name: 'Ask the gateway' }).click();
  await expect(page.locator('#host-hint')).toContainText('Asked the gateway at', { timeout: 30_000 });
});

test('a started session streams, echoes what is typed, and is listed', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByLabel('Prompt').fill('summarise the record');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(/\/session\.html\?id=[0-9a-f]{32}$/);
  await expect(page.locator('#status')).toHaveText('running');
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('claude: summarise the record');

  await page.locator('.xterm').click();
  await page.keyboard.type('hello from the browser');
  await page.keyboard.press('Enter');
  await expect(page.locator('.xterm-rows')).toContainText('hello from the browser');

  // a reload lands in the same place: the scrollback is the server's
  await page.reload();
  await expect(page.locator('.xterm-rows')).toContainText('hello from the browser');

  const id = new URL(page.url()).searchParams.get('id');
  await page.goto('/');
  const row = page.locator(`a[href="session.html?id=${id}"]`);
  await expect(row).toContainText('claude: summarise the record');
  await expect(row).toContainText('running for');
  await expect(row.getByRole('img', { name: 'running' })).toBeVisible();
});

test('the legacy address of a session opens the same page', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const id = new URL(page.url()).searchParams.get('id');
  await page.goto(`/s/${id}`);
  await expect(page).toHaveURL(new RegExp(`/session\\.html\\?id=${id}$`));
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
});

test('a killed session says it exited', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const id = new URL(page.url()).searchParams.get('id');
  // the page's own key, through the page's own client
  const status = await page.evaluate(
    async (sid) => (await (window as unknown as { amem: { fetch: (p: string, o: object) => Promise<Response> } })
      .amem.fetch(`/api/sessions/${sid}`, { method: 'DELETE' })).status,
    id,
  );
  expect(status).toBe(204);
  await expect(page.locator('#status')).toHaveText(/^exited/);
  await page.goto('/');
  await expect(page.locator(`a[href="session.html?id=${id}"]`)).toContainText('exited');
});

test('the preview quotes what a shell would read as a redirect', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.locator('#prompt').fill('fix>notes.md');
  await expect(page.locator('#cmd')).toContainText("--prompt 'fix>notes.md'");
});
