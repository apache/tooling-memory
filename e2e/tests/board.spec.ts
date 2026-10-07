// The board: a task filed here lands as proposed, and is admitted, claimed and completed
// with a handoff, each step moving it to its status's list; a task addressed to a person
// is marked as one.
import { test, expect } from './support/fixtures';

test('a task for a person is filed, admitted, claimed and completed with a handoff', async ({ signedIn: page }) => {
  await page.goto('/board');
  await page.getByLabel('Title').fill('Read the pane test handoff');
  await page.getByLabel('Requirements').fill('human');
  await page.getByLabel('Body').fill('Check **what** it says.');
  await page.getByRole('button', { name: 'File task' }).click();
  await expect(page.locator('#file-hint')).toContainText('Filed');

  const proposed = page.getByRole('region', { name: /^Proposed/ });
  const row = proposed.locator('.am-task', { hasText: 'Read the pane test handoff' });
  await expect(row).toContainText('For a person');
  await expect(row.getByRole('img', { name: 'proposed' })).toBeVisible();
  await row.getByRole('button', { name: 'Admit: Read the pane test handoff' }).click();

  const admitted = page.getByRole('region', { name: /^Admitted/ }).locator('.am-task', { hasText: 'Read the pane test handoff' });
  await admitted.getByRole('button', { name: /^Claim/ }).click();

  const claimed = page.getByRole('region', { name: /^Claimed/ }).locator('.am-task', { hasText: 'Read the pane test handoff' });
  await expect(claimed).toContainText('held by');
  await claimed.getByRole('button', { name: /^Complete/ }).click();
  const dialog = page.getByRole('dialog', { name: 'Complete with a handoff' });
  await dialog.getByLabel('Handoff').fill('Read it; nothing remains.');
  await dialog.getByRole('button', { name: 'Complete' }).click();

  const done = page.getByRole('region', { name: /^Completed/ }).locator('.am-task', { hasText: 'Read the pane test handoff' });
  await expect(done).toContainText('handoff mem-');
  await expect(done.getByRole('img', { name: 'completed' })).toBeVisible();
});

test('a task title is shown as text, never run as markup', async ({ signedIn: page }) => {
  await page.goto('/board');
  const title = '<img src=x onerror="window.pwned=1">';
  await page.getByLabel('Title').fill(title);
  await page.getByRole('button', { name: 'File task' }).click();
  await expect(page.locator('#lists')).toContainText(title);
  await expect(page.locator('#lists img')).toHaveCount(0);
  expect(await page.evaluate(() => (window as unknown as { pwned?: number }).pwned)).toBeUndefined();
});

test('an action the ledger refuses says so on the page', async ({ signedIn: page }) => {
  await page.goto('/board');
  await page.route('**/api/tasks/*/admit', (r) => r.fulfill({ status: 409, body: 'already admitted' }));
  await page.getByLabel('Title').fill('Refused on purpose');
  await page.getByRole('button', { name: 'File task' }).click();
  await page.getByRole('button', { name: 'Admit: Refused on purpose' }).click();
  await expect(page.getByRole('alert')).toContainText('already admitted');
});
