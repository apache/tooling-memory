// Every page at phone, tablet and desktop widths, light and dark: no sideways scroll, a
// screenshot of each for the pull request, and an accessibility scan.
import { test, expect, shot } from './support/fixtures';
import AxeBuilder from '@axe-core/playwright';

const widths = [360, 768, 1280] as const;
const pages = [
  { name: 'launch', path: '/' },
  { name: 'reader', path: '/reader' },
] as const;

for (const scheme of ['light', 'dark'] as const) {
  for (const p of pages) {
    test(`${p.name} fits every width in ${scheme}`, async ({ signedIn: page }) => {
      await page.emulateMedia({ colorScheme: scheme });
      for (const w of widths) {
        await page.setViewportSize({ width: w, height: 900 });
        await page.goto(p.path);
        await page.waitForLoadState('networkidle');
        const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
        expect(overflow, `${p.path} scrolls sideways at ${w}px`).toBeLessThanOrEqual(0);
        await page.screenshot({ path: shot(`${p.name}-${scheme}-${w}`), fullPage: true });
      }
    });

    test(`${p.name} has no serious accessibility violations in ${scheme}`, async ({ signedIn: page }) => {
      // known before the pages adopt the design book, whose Accessibility chapter lists
      // the pairs today's pages fail: the dark reader's Load button. The change that
      // applies the tokens removes this line, and the test says so if it does not.
      test.fail(p.name === 'reader' && scheme === 'dark', 'the dark reader\'s Load button fails contrast');
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto(p.path);
      await page.waitForLoadState('networkidle');
      const r = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa']).analyze();
      const serious = r.violations
        .filter((v) => v.impact === 'serious' || v.impact === 'critical')
        .map((v) => `${v.id}: ${v.nodes.map((n) => `${n.target.join(' ')} (${n.failureSummary ?? ''})`).join('; ')}`);
      expect(serious).toEqual([]);
    });
  }
}
