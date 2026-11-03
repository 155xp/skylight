// End-to-end benchmark in a real browser (headless Chromium through Playwright).
//
//   node scripts/bench.mjs [--base http://127.0.0.1:5490/] [--samples 30] [--seconds 10]
//                          [--key ~/.config/skylight/ticket-private.pem] [--aud <hostname>]
//                          [--screenshot out.png] [--json out.json] [--headed]
//                          [--modes click,key,move] [--no-motion] [--server-pid N] [--idle 10]
//
// It mints a ticket, opens the viewer, then measures:
//   * click-to-update, keypress-to-glyph and pointer-move latency through the probe window
//   * frame rate, bitrate and the capture-to-display breakdown while the probe animates, and
//     glass to glass: the probe stamps each redraw with a time code that is read back from the video
//   * with --server-pid, the server's CPU and what reaches the browser while the screen is still
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

for (const mode of arg('modes', 'click,key,move').split(',').filter(Boolean)) {
  const r = await page.evaluate(([mode, samples]) => window.skylight.measureLatency({ mode, samples }), [mode, samples]);
  result.latency[mode] = r;
  const f = (s) => `median ${s.median.toFixed(1)} ms, p95 ${s.p95.toFixed(1)}, min ${s.min.toFixed(1)}, max ${s.max.toFixed(1)}`;
  console.log(`${mode.padEnd(5)} input to decoded frame: ${f(r.decoded)}`);
  console.log(`${''.padEnd(5)} input to displayed:     ${f(r.shown)}  (${r.samples} samples, ${r.failed} lost)`);
}

if (!flag('no-motion')) {
  const glass = page.evaluate(([s, mode]) => window.skylight.measureGlass({ seconds: s, mode }), [seconds, arg('motion', 'animate')]);
  await page.waitForTimeout(1500);
  const stats = [];
  for (let i = 0; i < seconds - 1; i++) {
    await page.waitForTimeout(1000);
    stats.push(await page.evaluate(() => ({ ...window.skylight.stats })));
  }
  if (arg('screenshot')) await page.screenshot({ path: arg('screenshot') });
  result.glass = await glass;
  const avg = (k) => stats.reduce((a, s) => a + (s[k] || 0), 0) / stats.length;
  const keys = ['fps', 'kbps', 'latencyMs', 'latencyP95Ms', 'serverMs', 'queueMs', 'encodeMs', 'networkMs', 'decodeMs', 'displayMs', 'jitterMs', 'rttMs', 'targetKbps'];
  result.motion = Object.fromEntries(keys.map((k) => [k, Number(avg(k).toFixed(2))]));
  result.motion.encoder = stats.at(-1).encoder;
  const m = result.motion;
  const g = result.glass;
  console.log(`motion: ${m.fps.toFixed(0)} fps, ${(m.kbps / 1000).toFixed(1)} Mbit/s, capture to display ${m.latencyMs.toFixed(1)} ms (p95 ${m.latencyP95Ms.toFixed(1)})`);
  console.log(`        server ${m.serverMs.toFixed(1)} ms (capture to encode ${m.queueMs.toFixed(1)}, encode ${m.encodeMs.toFixed(1)}), network ${m.networkMs.toFixed(1)}, decode ${m.decodeMs.toFixed(1)}, to display ${m.displayMs.toFixed(1)}, rtt ${m.rttMs.toFixed(1)}`);
  if (g.samples) {
    console.log(`glass:  app draw to decoded median ${g.decoded.median.toFixed(1)} ms (p95 ${g.decoded.p95.toFixed(1)}), to displayed ${g.shown.median.toFixed(1)} ms (p95 ${g.shown.p95.toFixed(1)}), ${g.uniqueFps.toFixed(1)} distinct frames/s, ${g.rejected} torn`);
  } else {
    console.log('glass:  no time codes read');
  }
} else if (arg('screenshot')) {
  await page.screenshot({ path: arg('screenshot') });
}

if (arg('server-pid')) {
  // Idle: nothing on screen changes. Count server CPU time and what reaches the browser.
  const { readFileSync } = await import('node:fs');
  const cpu = () => {
    const f = readFileSync(`/proc/${arg('server-pid')}/stat`, 'utf8').split(') ')[1].split(' ');
    return (Number(f[11]) + Number(f[12])) / 100;
  };
  const idle = Number(arg('idle', 10));
  await page.waitForTimeout(2000);
  const rx0 = await page.evaluate(() => ({ ...window.skylight.received }));
  const c0 = cpu();
  const t = Date.now();
  await page.waitForTimeout(idle * 1000);
  const secs = (Date.now() - t) / 1000;
  const rx1 = await page.evaluate(() => ({ ...window.skylight.received }));
  result.idle = {
    serverCpuPercent: Number((((cpu() - c0) / secs) * 100).toFixed(2)),
    messagesPerSec: Number(((rx1.messages - rx0.messages) / secs).toFixed(2)),
    bytesPerSec: Number(((rx1.bytes - rx0.bytes) / secs).toFixed(1)),
  };
  console.log(`idle:   server CPU ${result.idle.serverCpuPercent}% of a core, ${result.idle.messagesPerSec} messages/s, ${result.idle.bytesPerSec} B/s to the browser`);
}

if (arg('json')) writeFileSync(arg('json'), JSON.stringify(result, null, 2));
await browser.close();
