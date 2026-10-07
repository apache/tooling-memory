// Every page at phone, tablet and desktop widths, light and dark: no sideways scroll, a
// screenshot of each for the pull request, and an accessibility scan.
import { test, expect, shot, startSession } from './support/fixtures';
import type { Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const widths = [360, 768, 1280] as const;
type Shown = { name: string; path: (id: string) => string; ready: (page: Page) => Promise<void> };
const pages: Shown[] = [
  { name: 'launch', path: () => '/', ready: async (page) => { await expect(page.locator('#ready li').first()).toBeVisible(); } },
  {
    name: 'session', path: (id) => `/session.html?id=${id}`,
    ready: async (page) => {
      await expect(page.locator('.xterm-rows')).toContainText('e2e session: ready');
      await expect(page.locator('#s-dest')).toContainText('llmao');
      // below 720px the record is a tab of its own; the terminal is what shows first
      if (await page.locator('#pane').isVisible()) {
        await page.locator('#recent a').first().click();
        await expect(page.locator('#emeta')).toBeVisible();
      }
    },
  },
  { name: 'board', path: () => '/board', ready: async (page) => { await expect(page.locator('#lists h2').first()).toBeVisible(); } },
  {
    name: 'reader', path: () => '/reader',
    ready: async (page) => {
      await page.getByRole('button', { name: 'dom-e2e', exact: true }).click();
      await page.locator('#q').fill('lease');
      await page.locator('#hits li button').first().click();
      await expect(page.locator('#entry h2')).toBeVisible();
    },
  },
];

for (const scheme of ['light', 'dark'] as const) {
  for (const p of pages) {
    test(`${p.name} fits every width in ${scheme}`, async ({ signedIn: page, request }) => {
      const id = await startSession(request);
      await page.emulateMedia({ colorScheme: scheme });
      for (const w of widths) {
        await page.setViewportSize({ width: w, height: 900 });
        await page.goto(p.path(id));
        await p.ready(page);
        const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
        expect(overflow, `${p.name} scrolls sideways at ${w}px`).toBeLessThanOrEqual(0);
        await page.screenshot({ path: shot(`${p.name}-${scheme}-${w}`), fullPage: true });
      }
    });

    test(`${p.name} has no serious accessibility violations in ${scheme}`, async ({ signedIn: page, request }) => {
      const id = await startSession(request);
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto(p.path(id));
      await p.ready(page);
      const r = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
      const serious = r.violations
        .filter((v) => v.impact === 'serious' || v.impact === 'critical')
        .map((v) => `${v.id}: ${v.nodes.map((n) => `${n.target.join(' ')} (${n.failureSummary ?? ''})`).join('; ')}`);
      expect(serious).toEqual([]);
    });
  }
}

// more contrast, as the system asks for it: the same scan, and a screenshot of each page
for (const p of pages) {
  test(`${p.name} with more contrast has no serious accessibility violations`, async ({ signedIn: page, request }) => {
    const id = await startSession(request);
    await page.emulateMedia({ contrast: 'more' });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(p.path(id));
    await p.ready(page);
    await page.screenshot({ path: shot(`${p.name}-more-1280`), fullPage: true });
    const r = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa']).analyze();
    expect(r.violations.filter((v) => v.impact === 'serious' || v.impact === 'critical').map((v) => v.id)).toEqual([]);
  });
}
