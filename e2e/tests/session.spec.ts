// The session page's frame: the bar says what the session was started with and how it
// stands, and the strip says what crosses the line, from the status file the launcher
// writes (e2e/session.sh writes one, as the launcher would).
import { test, expect, sessionInfo } from './support/fixtures';

test('the bar names the harness, the model and the profile', async ({ signedIn: page }) => {
  await page.locator('#profile').selectOption('claude');
  await page.locator('#model').fill('qwen3.8-27b');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('#status')).toHaveText('running');
  const facts = page.getByRole('definition');
  await expect(page.locator('#facts')).toContainText('Harness');
  await expect(facts.filter({ hasText: 'claude' }).first()).toBeVisible();
  await expect(facts.filter({ hasText: 'qwen3.8-27b' })).toHaveCount(1);
  await expect(page.locator('#facts')).toContainText('Profile');
  const info = await sessionInfo(page);
  expect(info.launch.profile).toBe('claude');
  expect(info.launch.model).toBe('qwen3.8-27b');
});

test('the strip says where calls go, the label against the ceiling, usage and what was written', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  const strip = page.getByRole('group', { name: 'What crosses the line' });
  await expect(strip).toBeVisible();
  await expect(page.locator('#s-dest')).toContainText('llmao');
  await expect(page.locator('#s-dest')).toContainText('gateway');
  // the label it has read is the ceiling: a warning, with its word
  await expect(page.locator('#s-label')).toContainText('internal');
  await expect(page.locator('#s-label')).toContainText('of internal');
  await expect(page.locator('#s-label .am-chip--warn')).toHaveAttribute('title', 'at ceiling');
  // past 10,000 a count abbreviates, and is exact in the title; under it, it is exact
  await expect(page.locator('#s-usage')).toContainText('41.2k in, 3120 out');
  await expect(page.locator('#s-usage')).toContainText('12 calls');
  await expect(page.locator('#s-usage')).toHaveAttribute('title', /41234 tokens in/);
  await expect(page.locator('#s-written')).toHaveText('3');
  await expect(page.locator('#s-handoffs .am-chip--await')).toHaveAttribute('title', '1 to read');
});

test('a session the launcher has said nothing about says so, rather than zero', async ({ signedIn: page }) => {
  // a page opened on an id with no status file: everything not reported
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('#status')).toHaveText('running');
  const id = new URL(page.url()).searchParams.get('id');
  await page.route(`**/api/sessions/${id}/status`, (r) => r.fulfill({ json: {} }));
  await page.reload();
  await expect(page.locator('#s-usage')).toHaveText('not reported');
  await expect(page.locator('#s-written')).toHaveText('not reported');
});
