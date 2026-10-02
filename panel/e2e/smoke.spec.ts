import { expect, test } from '@playwright/test';

// Runs against a build made with VITE_API=mock (see playwright.config.ts).
test('overview loads and navigation works', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Overview' })).toBeVisible();
  await page.getByRole('link', { name: 'Search playground' }).click();
  await expect(page.getByRole('heading', { name: 'Search playground' })).toBeVisible();
});
