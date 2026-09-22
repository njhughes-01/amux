import { test, expect } from './fixtures';

test('Browser Trail renders attributed actions without sensitive inputs', async ({ page }) => {
  await page.route('**/api/browser/profiles', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ profiles: [], chrome_profiles: [], backends: ['native'] }),
  }));
  await page.route('**/api/browser/history?**', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({
      session: 'amux',
      measured: true,
      n_considered: 2,
      returned: 2,
      truncated: false,
      sensitive_values_recorded: false,
      events: [
        {
          at: '2026-09-06T07:12:03Z',
          event: 'browser.action',
          data: {
            action: 'type',
            typed_chars: 18,
            url: 'https://example.com/login',
            http_status: 200,
            sensitive_values_recorded: false,
          },
        },
        {
          at: '2026-09-06T07:11:54Z',
          event: 'browser.navigated',
          data: { url: 'https://example.com/login', ready_state: 'complete' },
        },
      ],
    }),
  }));

  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._bwInspTab === 'function');
  await page.evaluate(() => (window as any).switchView('browser'));
  await page.locator('#bw-inspect-btn').click();
  await page.locator('button[data-itab="trail"]').click();

  const panel = page.locator('#bw-inspect-list');
  await expect(panel).toContainText('type');
  await expect(panel).toContainText('18 chars (contents withheld)');
  await expect(panel).toContainText('https://example.com/login');
  await expect(page.locator('#bw-ic-trail')).toHaveText('(2)');
  await expect(panel).not.toContainText('hunter2');
  console.log('[browser-trail] measured=true returned=2/2 sensitive_values_recorded=false');
});

test('Browser Trail distinguishes measured-empty history', async ({ page }) => {
  await page.route('**/api/browser/profiles', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ profiles: [], chrome_profiles: [], backends: ['native'] }),
  }));
  await page.route('**/api/browser/history?**', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({
      session: 'amux', measured: true, n_considered: 0, returned: 0,
      truncated: false, sensitive_values_recorded: false, events: [],
    }),
  }));

  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._bwInspTab === 'function');
  await page.evaluate(() => (window as any).switchView('browser'));
  await page.locator('#bw-inspect-btn').click();
  await page.locator('button[data-itab="trail"]').click();

  await expect(page.locator('#bw-inspect-list')).toContainText(
    'No recorded browser actions for this worker yet.',
  );
  console.log('[browser-trail-empty] measured=true n_considered=0');
});

test('Inspector preserves Trail while a late response updates the active tab', async ({ page }) => {
  let releaseInspect!: () => void;
  const inspectReady = new Promise<void>(resolve => { releaseInspect = resolve; });
  await page.route('**/api/browser/profiles', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ profiles: [], chrome_profiles: [], backends: ['native'] }),
  }));
  await page.route('**/api/browser/inspect?**', async route => {
    await inspectReady;
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        console: [{ level: 'info', text: 'console entry' }],
        network: [{ ok: true, status: 200, method: 'GET', url: 'https://example.com/api' }],
        errors: [],
        counts: { console: 1, network: 1, errors: 0 },
      }),
    });
  });
  await page.route('**/api/browser/history?**', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({
      session: 'amux', measured: true, n_considered: 1, returned: 1,
      truncated: false, sensitive_values_recorded: false,
      events: [{ at: '2026-09-06T07:12:03Z', event: 'browser.navigated', data: { url: 'https://example.com' } }],
    }),
  }));

  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._bwInspTab === 'function');
  await page.evaluate(() => (window as any).switchView('browser'));
  await page.locator('#bw-inspect-btn').click();
  await page.locator('button[data-itab="trail"]').click();
  await expect(page.locator('#bw-inspect-list')).toContainText('https://example.com');
  await page.locator('button[data-itab="network"]').click();
  releaseInspect();
  await expect(page.locator('#bw-inspect-list')).toContainText('https://example.com/api');
  await page.locator('button[data-itab="trail"]').click();
  await expect(page.locator('#bw-inspect-list')).toContainText('https://example.com');
});
