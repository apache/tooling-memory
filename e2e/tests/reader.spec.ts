// The browser reader over the record's own domains: one click exports a domain, the page
// verifies and indexes it, and a search ranks the entry with the query term first.
import { test, expect } from './support/fixtures';

test('the reader loads a domain and finds an entry', async ({ signedIn: page }) => {
  await page.goto('/reader');
  await page.getByRole('button', { name: 'dom-e2e', exact: true }).click();
  await expect(page.locator('#status')).toContainText(/\d+ entr/);
  await page.locator('#q').fill('horizon');
  await expect(page.locator('#hits li').first()).toContainText('Leases expire at a fixed horizon');
});
