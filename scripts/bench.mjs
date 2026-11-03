// End-to-end benchmark in a real browser (headless Chromium through Playwright).
//
//   node scripts/bench.mjs [--base http://127.0.0.1:5490/] [--samples 30] [--seconds 10]
//                          [--key ~/.config/skylight/ticket-private.pem] [--aud <hostname>]
//                          [--screenshot out.png] [--json out.json] [--headed]
//                          [--modes click,key,move] [--no-motion]
//
// It mints a ticket, opens the viewer, then measures:
//   * click-to-update, keypress-to-glyph and pointer-move latency through the probe window
//   * frame rate, bitrate and the capture-to-display breakdown while the probe animates
import { chromium } from 'playwright-core';
import { homedir, hostname } from 'node:os';
import { writeFileSync } from 'node:fs';
import { loadPrivateKey, mintTicket } from '../clients/node/mint-ticket.mjs';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : def;
};
const flag = (name) => process.argv.includes(`--${name}`);
const base = arg('base', 'http://127.0.0.1:5490/');
const samples = Number(arg('samples', 30));
const seconds = Number(arg('seconds', 10));
const keyPath = arg('key', `${homedir()}/.config/skylight/ticket-private.pem`);
const aud = arg('aud', hostname());

const ticket = mintTicket(loadPrivateKey(keyPath), { aud, sub: 'bench', scope: ['view', 'input', 'clipboard'] });
const browser = await chromium.launch({
  executablePath: arg('chromium', '/usr/bin/chromium'),
  headless: !flag('headed'),
  args: ['--autoplay-policy=no-user-gesture-required', '--disable-features=CalculateNativeWinOcclusion'],
});
const page = await browser.newPage({ viewport: { width: 1600, height: 900 } });
page.on('console', (m) => {
  if (m.type() === 'error' || m.type() === 'warning') console.error('[page]', m.text());
});
const t0 = Date.now();
// Tunnels sometimes reset a fresh connection; retry the page load (the ticket is not spent yet).
for (let attempt = 1; ; attempt++) {
  try {
    await page.goto(`${base}#ticket=${ticket}&stats`);
    break;
  } catch (e) {
    if (attempt >= 4) throw e;
    console.error(`page load failed (${e.message.split('\n')[0].replace(/ticket=[^&\s)]+/, 'ticket=<redacted>')}), retrying`);
    await new Promise((r) => setTimeout(r, 500));
  }
}
await page.waitForFunction(() => window.skylight && window.skylight.state === 'connected', null, { timeout: 15000 });
const connectMs = Date.now() - t0;
await page.waitForFunction(() => window.skylight.width > 0, null, { timeout: 10000 });
await page.waitForTimeout(1500);

const result = { base, when: new Date().toISOString(), connectMs, latency: {}, motion: null };
const info = await page.evaluate(() => ({
  width: window.skylight.width,
  height: window.skylight.height,
  codec: window.skylight.decoderConfig && window.skylight.decoderConfig.codec,
  refreshHz: window.skylight.refreshHz,
  userAgent: navigator.userAgent,
  hardwareConcurrency: navigator.hardwareConcurrency,
}));
Object.assign(result, info);
console.log(`connected in ${connectMs} ms: ${info.width}x${info.height} ${info.codec}, page refresh ${info.refreshHz} Hz`);

for (const mode of arg('modes', 'click,key,move').split(',')) {
  const r = await page.evaluate(([mode, samples]) => window.skylight.measureLatency({ mode, samples }), [mode, samples]);
  result.latency[mode] = r;
  const f = (s) => `median ${s.median.toFixed(1)} ms, p95 ${s.p95.toFixed(1)}, min ${s.min.toFixed(1)}, max ${s.max.toFixed(1)}`;
  console.log(`${mode.padEnd(5)} input to decoded frame: ${f(r.decoded)}`);
  console.log(`${''.padEnd(5)} input to displayed:     ${f(r.shown)}  (${r.samples} samples, ${r.failed} lost)`);
}

if (!flag('no-motion')) {
  await page.evaluate(() => window.skylight.showProbe('animate'));
  await page.waitForTimeout(1500);
  const stats = [];
  for (let i = 0; i < seconds; i++) {
    await page.waitForTimeout(1000);
    stats.push(await page.evaluate(() => ({ ...window.skylight.stats })));
  }
  if (arg('screenshot')) await page.screenshot({ path: arg('screenshot') });
  await page.evaluate(() => window.skylight.showProbe(null));
  const avg = (k) => stats.reduce((a, s) => a + (s[k] || 0), 0) / stats.length;
  const keys = ['fps', 'kbps', 'latencyMs', 'latencyP95Ms', 'serverMs', 'queueMs', 'encodeMs', 'networkMs', 'decodeMs', 'displayMs', 'jitterMs', 'rttMs', 'targetKbps'];
  result.motion = Object.fromEntries(keys.map((k) => [k, Number(avg(k).toFixed(2))]));
  result.motion.encoder = stats.at(-1).encoder;
  const m = result.motion;
  console.log(`motion: ${m.fps.toFixed(0)} fps, ${(m.kbps / 1000).toFixed(1)} Mbit/s, capture to display ${m.latencyMs.toFixed(1)} ms (p95 ${m.latencyP95Ms.toFixed(1)})`);
  console.log(`        server ${m.serverMs.toFixed(1)} ms (capture to encode ${m.queueMs.toFixed(1)}, encode ${m.encodeMs.toFixed(1)}), network ${m.networkMs.toFixed(1)}, decode ${m.decodeMs.toFixed(1)}, to display ${m.displayMs.toFixed(1)}, rtt ${m.rttMs.toFixed(1)}`);
} else if (arg('screenshot')) {
  await page.screenshot({ path: arg('screenshot') });
}

if (arg('json')) writeFileSync(arg('json'), JSON.stringify(result, null, 2));
await browser.close();
