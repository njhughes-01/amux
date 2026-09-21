import { test, expect } from './fixtures';

// LC-29: answering a needs-you card must (1) let the owner TYPE a full answer
// (focus shortcuts used to fire on the second key), and (2) actually move the
// card: the dashboard used to leave it in `needsyou`, so it came straight back.
// Cards carry no worker, so nothing is woken; the real server records the
// answer and the real dashboard drives it.

async function parkedCard(request: any, auth: Record<string, string>, title: string): Promise<string> {
  const created = await request.post('/api/board', { headers: auth, data: { title, status: 'backlog', type: 'chore' } });
  expect(created.ok(), await created.text()).toBeTruthy();
  const id = (await created.json()).id as string;
  const parked = await request.patch(`/api/board/${encodeURIComponent(id)}`, { headers: auth, data: {
    status: 'needsyou', ask_type: 'decision', ask_actor: 'the owner',
    ask_question: 'Which port should it use?', ask_unblocks: 'the owner answers',
  } });
  expect(parked.ok(), await parked.text()).toBeTruthy();
  return id;
}

async function card(request: any, auth: Record<string, string>, id: string) {
  return (await request.get(`/api/board/${encodeURIComponent(id)}`, { headers: auth })).json();
}

test('a typed Focus answer keeps every key and moves the card out of needs-you', async ({ page, request }) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const id = await parkedCard(request, auth, 'focus answer e2e subject');
  const owner = await page.evaluate(() => (window as any)._ownerName());

  await page.evaluate(async () => { await (window as any).fetchBoard(); (window as any)._focusStart('is:needsyou'); });
  await expect(page.locator('#focus-overlay')).toContainText(id);
  await page.keyboard.press('e');
  const box = page.locator('#focus-ans');
  await expect(box).toBeVisible();
  // Every former shortcut letter (a r n e j k u) in one answer.
  const answer = 'use port 8824, run it and ask me again only if unsure';
  await box.pressSequentially(answer);
  await expect(box).toHaveValue(answer);
  expect((await card(request, auth, id)).status, 'typing must not approve or reject').toBe('needsyou');
  await page.locator('#modal-btns').getByRole('button', { name: 'Send', exact: true }).click();

  await expect.poll(async () => (await card(request, auth, id)).status).toBe('todo');
  const after = await card(request, auth, id);
  expect(after.log).toContain(`\`answered\` ${owner}: ${answer}`);
  expect(after.ask_question).toBeNull();
  await expect(page.locator('#focus-overlay')).not.toContainText(id);
});

test('the card detail Approve button records the decision and returns the card to its queue', async ({ page, request }) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const id = await parkedCard(request, auth, 'detail approve e2e subject');
  const owner = await page.evaluate(() => (window as any)._ownerName());

  await page.goto(`/#issue=${encodeURIComponent(id)}`);
  await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  const actions = page.locator('.bd-ask-actions');
  await expect(actions).toBeVisible();
  await actions.getByRole('button', { name: 'Approve', exact: true }).click();

  await expect.poll(async () => (await card(request, auth, id)).status).toBe('todo');
  expect((await card(request, auth, id)).log).toContain(`\`decision\` ${owner} APPROVED`);
  // Answered once; the buttons go away with the ask.
  await expect(page.locator('.bd-ask-actions')).toHaveCount(0);
});
