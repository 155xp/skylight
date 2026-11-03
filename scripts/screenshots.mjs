// Viewer screenshots: desktop with stats and settings open, and a phone with touch emulation.
//   node scripts/screenshots.mjs [--base http://127.0.0.1:5490/] [--out docs/results]
//                                [--key ~/.config/skylight/ticket-private.pem] [--aud <hostname>]
import { chromium } from 'playwright-core';
import { homedir, hostname } from 'node:os';
import { loadPrivateKey, mintTicket } from '../clients/node/mint-ticket.mjs';

const arg = (n, d) => (process.argv.indexOf(`--${n}`) > 0 ? process.argv[process.argv.indexOf(`--${n}`) + 1] : d);
const base = arg('base', 'http://127.0.0.1:5490/');
const out = arg('out', 'docs/results');
const key = loadPrivateKey(arg('key', `${homedir()}/.config/skylight/ticket-private.pem`));
const ticket = () => mintTicket(key, { aud: arg('aud', hostname()), sub: 'screenshots', scope: ['view'] });
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium' });

async function shot(name, context, prepare) {
  const page = await context.newPage();
  await page.goto(`${base}#ticket=${ticket()}`);
  await page.waitForFunction(() => window.skylight?.state === 'connected' && window.skylight.width > 0);
  await page.waitForTimeout(2500);
  if (prepare) await prepare(page);
  await page.screenshot({ path: `${out}/${name}.png` });
  await page.close();
}

const desktop = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: 'light' });
await shot('viewer-desktop', desktop, async (page) => {
  // The toolbar hides while you work; bring it back from the top edge like a user would.
  await page.mouse.move(720, 30);
  await page.mouse.move(720, 10);
  await page.click('#statsbtn');
  await page.mouse.move(720, 10);
  await page.click('#settingsbtn');
  await page.waitForTimeout(1200);
});
const phone = await browser.newContext({
  viewport: { width: 393, height: 852 }, deviceScaleFactor: 3, isMobile: true, hasTouch: true, colorScheme: 'dark',
  userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1',
});
await shot('viewer-phone', phone, async (page) => page.waitForTimeout(1000));
await browser.close();
console.log(`wrote ${out}/viewer-desktop.png and ${out}/viewer-phone.png`);
