// The reading pane beside the terminal: an entry the session writes opens as it lands,
// and the recent entries are listed under it.
import { test, expect, ids, root } from './support/fixtures';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';

const amem = process.env.AMEM ?? join(__dirname, '..', '..', 'target', 'debug', 'amem');

test('an entry written during the session opens in the pane as it lands', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  // the seeded entries are listed, newest first
  await expect(page.locator('#recent')).toContainText('Leases expire at a fixed horizon');

  execFileSync(amem, ['--root', root, 'write', '--kind', 'handoff', '--title', 'Stopped at the pane test',
    '--body', 'Next: **the status strip**.'], { cwd: '/tmp' });
  await expect(page.locator('#ptitle')).toHaveText('Stopped at the pane test');
  await expect(page.locator('#body strong')).toHaveText('the status strip');
});

test('a recent entry opens when clicked', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start' }).click();
  await page.locator('#recent a', { hasText: 'The ledger\'s clock is its own' }).click();
  await expect(page.locator('#ptitle')).toHaveText('The ledger\'s clock is its own');
  await expect(page.locator('#kind')).toContainText('fact');
  expect(ids()).toHaveLength(3);
});

test('a title is shown as text, never run as markup', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const title = '<img src=x onerror="window.pwned=1">';
  execFileSync(amem, ['--root', root, 'write', '--kind', 'fact', '--title', title, '--body', 'x'], { cwd: '/tmp' });
  await expect(page.locator('#recent')).toContainText(title);
  await expect(page.locator('#recent img')).toHaveCount(0);
  expect(await page.evaluate(() => (window as unknown as { pwned?: number }).pwned)).toBeUndefined();
});
