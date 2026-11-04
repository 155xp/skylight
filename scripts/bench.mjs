// End-to-end benchmark in a real browser (headless Chromium through Playwright).
//
//   node scripts/bench.mjs [--base http://127.0.0.1:5490/] [--samples 30] [--seconds 10]
//                          [--key ~/.config/skylight/ticket-private.pem] [--aud <hostname>]
//                          [--screenshot out.png] [--json out.json] [--headed]
//                          [--modes click,key,move] [--no-motion] [--server-pid N] [--x-pid N]
//                          [--idle 10]
//
// It mints a ticket, opens the viewer, then measures:
//   * click-to-update, keypress-to-glyph and pointer-move latency through the probe window
//   * frame rate, bitrate and the capture-to-display breakdown while the probe animates, and
//     glass to glass: the probe stamps each redraw with a time code that is read back from the video
//   * frame pacing under motion: the spread of the intervals between decoded frames, hitches (a
//     gap of more than two intervals) and the frames the server's pacer skipped
//   * with --server-pid (and --x-pid for the X server it captures), CPU use under motion, and
//     the server's CPU and what reaches the browser while the screen is still
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
    await page.goto(`${base}#ticket=${ticket}&stats${flag('no-worker') ? '&noworker' : ''}`);
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
if (arg('jank')) {
  // A busy page: block the page's thread for N ms out of every 50 (heavy layout, charts, GC).
  await page.evaluate((ms) => {
    setInterval(() => {
      const end = performance.now() + ms;
      while (performance.now() < end);
    }, 50);
  }, Number(arg('jank')));
}

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

// CPU seconds (user + system) a process has used so far, from /proc.
const cpuSecs = async (pid) => {
  const { readFileSync } = await import('node:fs');
  const f = readFileSync(`/proc/${pid}/stat`, 'utf8').split(') ')[1].split(' ');
  return (Number(f[11]) + Number(f[12])) / 100;
};
// Processes whose CPU use is reported under motion and while idle: the server, and optionally the
// X server it captures from (whose share of the work is the readback).
const pids = [['server', arg('server-pid')], ['X server', arg('x-pid')]].filter(([, p]) => p);

if (!flag('no-motion')) {
  const glass = page.evaluate(([s, mode]) => window.skylight.measureGlass({ seconds: s, mode }), [seconds, arg('motion', 'animate')]);
  await page.waitForTimeout(1500);
  // Every decoded frame's time, for frame pacing: what the eye sees as smooth or stuttering.
  await page.evaluate(() => {
    const view = window.skylight;
    const log = (view._benchFrames = []);
    const sink = view._onSinkEvent.bind(view);
    view._onSinkEvent = (m) => {
      if (m.t === 'frame' && !m.refine) log.push(m.decoded);
      sink(m);
    };
  });
  const server0 = await page.evaluate(() => ({ ...window.skylight.serverStats }));
  const cpu0 = await Promise.all(pids.map(([, p]) => cpuSecs(p)));
  const motionStart = Date.now();
  const stats = [];
  for (let i = 0; i < seconds - 1; i++) {
    await page.waitForTimeout(1000);
    stats.push(await page.evaluate(() => ({ ...window.skylight.stats })));
  }
  const motionSecs = (Date.now() - motionStart) / 1000;
  const cpu1 = await Promise.all(pids.map(([, p]) => cpuSecs(p)));
  const server1 = await page.evaluate(() => ({ ...window.skylight.serverStats }));
  const decodedAt = await page.evaluate(() => window.skylight._benchFrames.splice(0));
  if (arg('screenshot')) await page.screenshot({ path: arg('screenshot') });
  result.glass = await glass;
  const iv = decodedAt.slice(1).map((t, i) => t - decodedAt[i]).sort((a, b) => a - b);
  const q = (p) => (iv.length ? iv[Math.min(iv.length - 1, Math.round((iv.length - 1) * p))] : 0);
  const mean = iv.reduce((a, b) => a + b, 0) / Math.max(1, iv.length);
  result.pacing = {
    frames: decodedAt.length,
    intervalMs: { median: q(0.5), p95: q(0.95), p99: q(0.99), max: q(1) },
    stdevMs: Math.sqrt(iv.reduce((a, x) => a + (x - mean) ** 2, 0) / Math.max(1, iv.length)),
    // A frame that comes more than twice the usual interval after the one before it is a visible
    // hitch.
    hitches: iv.filter((x) => x > 2 * q(0.5)).length,
    skipped: (server1.skipped || 0) - (server0.skipped || 0),
    stalls: (server1.stalls || 0) - (server0.stalls || 0),
  };
  result.cpu = Object.fromEntries(pids.map(([name], i) => [name, Number((((cpu1[i] - cpu0[i]) / motionSecs) * 100).toFixed(1))]));
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
  const pc = result.pacing;
  console.log(`pacing: frame interval median ${pc.intervalMs.median.toFixed(1)} ms, p95 ${pc.intervalMs.p95.toFixed(1)}, p99 ${pc.intervalMs.p99.toFixed(1)}, max ${pc.intervalMs.max.toFixed(1)}, jitter ${pc.stdevMs.toFixed(1)} ms, ${pc.hitches} hitches, ${pc.skipped} skipped, ${pc.stalls} stalls`);
  if (pids.length) console.log(`cpu:    ${pids.map(([name]) => `${name} ${result.cpu[name]}%`).join(', ')} of a core under motion`);
} else if (arg('screenshot')) {
  await page.screenshot({ path: arg('screenshot') });
}

if (arg('server-pid')) {
  // Idle: nothing on screen changes. Count server CPU time and what reaches the browser.
  const idle = Number(arg('idle', 10));
  await page.waitForTimeout(2000);
  const rx0 = await page.evaluate(() => ({ ...window.skylight.received }));
  const c0 = await cpuSecs(arg('server-pid'));
  const t = Date.now();
  await page.waitForTimeout(idle * 1000);
  const secs = (Date.now() - t) / 1000;
  const rx1 = await page.evaluate(() => ({ ...window.skylight.received }));
  result.idle = {
    serverCpuPercent: Number((((await cpuSecs(arg('server-pid'))) - c0) / secs * 100).toFixed(2)),
    messagesPerSec: Number(((rx1.messages - rx0.messages) / secs).toFixed(2)),
    bytesPerSec: Number(((rx1.bytes - rx0.bytes) / secs).toFixed(1)),
  };
  console.log(`idle:   server CPU ${result.idle.serverCpuPercent}% of a core, ${result.idle.messagesPerSec} messages/s, ${result.idle.bytesPerSec} B/s to the browser`);
}

if (arg('json')) writeFileSync(arg('json'), JSON.stringify(result, null, 2));
await browser.close();
