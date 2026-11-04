// End-to-end benchmark in a real browser (headless Chromium through Playwright).
//
//   node scripts/bench.mjs [--base http://127.0.0.1:5490/] [--samples 30] [--seconds 10]
//                          [--key ~/.config/skylight/ticket-private.pem] [--aud <hostname>]
//                          [--screenshot out.png] [--json out.json] [--headed]
//                          [--modes click,key,move] [--no-motion] [--server-pid N] [--x-pid N]
//                          [--idle 10] [--connects N] [--quality N] [--mbps M] [--no-smooth]
//
// It mints a ticket, opens the viewer, then measures:
//   * click-to-update, keypress-to-glyph and pointer-move latency through the probe window
//   * frame rate, bitrate and the capture-to-display breakdown while the probe animates, and
//     glass to glass: the probe stamps each redraw with a time code that is read back from the video
//     (--motion animate, scroll, or follow: a square that follows the pointer, which the bench
//     moves in circles, like dragging)
//   * frame pacing under motion: the spread of the intervals between decoded frames, hitches (a
//     gap of more than two intervals) and the frames the server's pacer skipped
//   * with --server-pid (and --x-pid for the X server it captures), CPU use under motion, and
//     the server's CPU and what reaches the browser while the screen is still
//   * with --quality N, N changes of a page of text: what the browser shows against the server's
//     own picture of it (luma PSNR), right after the change and once it has settled, and what the
//     change cost; --mbps caps the bitrate so such runs compare at the same budget
//   * with --connects N, N fresh connections: time to the welcome and to the first frame, and
//     from a dropped socket to the first frame after the client reconnects on its own
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
    await page.goto(`${base}#ticket=${ticket}&stats${flag('no-worker') ? '&noworker' : ''}${flag('no-smooth') ? '&nosmooth' : ''}`);
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
if (arg('mbps')) {
  // A fixed bitrate cap (Mbit/s) instead of whatever the controller has reached, for comparisons
  // that depend on the budget, such as image quality.
  await page.evaluate((b) => window.skylight.setOptions({ maxBitrate: b }), Number(arg('mbps')) * 1e6);
}
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
  if (arg('motion') === 'follow') {
    // The follow probe moves only when the pointer does, as a drag or a scroll does: drive the
    // pointer in circles over it, one turn a second at a mouse's 250 updates a second, all run.
    const rect = await page.evaluate(() => window.skylight.showProbe('follow'));
    await page.evaluate(([px, py, pw, ph]) => {
      const view = window.skylight;
      const t0 = performance.now();
      view._benchDrive = setInterval(() => {
        const a = ((performance.now() - t0) / 1000) * 2 * Math.PI;
        view._pointer(px + pw / 2 + Math.cos(a) * pw * 0.3, py + ph / 2 + Math.sin(a) * ph * 0.3);
      }, 4);
    }, rect);
  }
  const glass = page.evaluate(([s, mode]) => window.skylight.measureGlass({ seconds: s, mode }), [seconds, arg('motion', 'animate')]);
  await page.waitForTimeout(1500);
  // Every frame's decode and draw time, and every display refresh, for frame pacing: what the eye
  // sees as smooth or stuttering.
  await page.evaluate(() => {
    const view = window.skylight;
    const log = (view._benchFrames = { decoded: [], drawn: [], refresh: [] });
    const sink = view._onSinkEvent.bind(view);
    view._onSinkEvent = (m) => {
      if (m.t === 'frame' && !m.refine) {
        log.decoded.push(m.decoded);
        // A client that holds a frame for the next refresh reports when it drew it, or null
        // for one a newer frame replaced; an older one draws every frame on decode.
        if (m.drawn !== null) log.drawn.push(('drawn' in m ? m.drawn : m.decoded) - performance.timeOrigin);
      }
      sink(m);
    };
    const tick = (t) => {
      if (view._benchFrames !== log) return;
      log.refresh.push(t);
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
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
  const frameLog = await page.evaluate(() => {
    const f = window.skylight._benchFrames;
    window.skylight._benchFrames = null;
    return f;
  });
  const decodedAt = frameLog.decoded;
  if (arg('screenshot')) await page.screenshot({ path: arg('screenshot') });
  result.glass = await glass;
  await page.evaluate(() => clearInterval(window.skylight._benchDrive));
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
  // On screen: a refresh shows the last frame drawn before it, so of two frames drawn between the
  // same two refreshes the first is never seen, and a refresh with none repeats the frame before.
  {
    const r = frameLog.refresh;
    const drawn = frameLog.drawn.filter((t) => t >= r[0] && t < r.at(-1)).sort((a, b) => a - b);
    let j = 0;
    let shown = 0;
    let repeats = 0;
    let gap = 0;
    for (let k = 0; k + 1 < r.length && j < drawn.length; k++) {
      let n = 0;
      while (j < drawn.length && drawn[j] < r[k + 1]) {
        n++;
        j++;
      }
      if (n > 0) {
        shown++;
        if (shown > 1) repeats += gap;
        gap = 0;
      } else if (shown > 0) {
        gap++;
      }
    }
    const replaced = frameLog.decoded.length - frameLog.drawn.length;
    result.pacing.display = { frames: drawn.length + replaced, neverShown: drawn.length - shown + replaced, repeats, refreshes: r.length };
  }
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
  const ds = pc.display;
  console.log(`screen: ${ds.neverShown} of ${ds.frames} frames never shown (two drawn in one refresh), ${ds.repeats} refreshes repeated a frame, of ${ds.refreshes}`);
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

if (Number(arg('quality', 0)) > 0) {
  // Image quality per bit. The page probe turns to a new page of text and grey panels on each
  // click (the whole 960 px window changes at once) and then stays still. For each change, the
  // decoded frames are compared with the server's own picture of the settled screen
  // (/api/screenshot): luma PSNR of the first frame of the new page, which the encoder had to fit
  // under its cap, and of the last one, after the refinement frames; and what the change cost.
  const key = loadPrivateKey(keyPath);
  await page.exposeFunction('benchViewTicket', () => mintTicket(key, { aud, sub: 'bench', scope: ['view'] }));
  const runs = await page.evaluate(async (pages) => {
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    const view = window.skylight;
    const [px, py, pw, ph] = await view.showProbe('page');
    const canvas = new OffscreenCanvas(view.width, view.height);
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    const lumaOf = (img) => {
      ctx.drawImage(img, 0, 0, view.width, view.height);
      const d = ctx.getImageData(px, py, pw, ph).data;
      const y = new Float32Array(pw * ph);
      for (let i = 0; i < y.length; i++) y[i] = 0.299 * d[i * 4] + 0.587 * d[i * 4 + 1] + 0.114 * d[i * 4 + 2];
      return y;
    };
    const psnr = (a, b) => {
      let se = 0;
      for (let i = 0; i < a.length; i++) se += (a[i] - b[i]) ** 2;
      return se === 0 ? 99 : 10 * Math.log10((255 * 255 * a.length) / se);
    };
    const screenshot = async () => {
      const r = await fetch(new URL('api/screenshot', location.href), { headers: { Authorization: `Bearer ${await window.benchViewTicket()}` } });
      return lumaOf(await createImageBitmap(await r.blob()));
    };
    let frames = null;
    const sink = view._onSinkEvent.bind(view);
    view._onSinkEvent = (m) => {
      if (m.t === 'vframe' && frames) frames.push({ at: performance.now(), luma: lumaOf(m.frame), bytes: m.meta ? m.meta.bytes : 0 });
      sink(m);
    };
    view._watchFrames(true);
    await sleep(1500);
    let before = await screenshot();
    const out = [];
    for (let n = 0; n < pages; n++) {
      frames = [];
      view._pointer(px + pw / 2, py + ph / 2);
      view._button(0, true);
      view._button(0, false);
      // Settled: nothing new for 800 ms (the refinements come 120 ms after the change, one per
      // round trip).
      const start = performance.now();
      while (performance.now() - start < 8000) {
        await sleep(100);
        if (frames.length && performance.now() - frames.at(-1).at > 800) break;
      }
      const got = frames;
      frames = null;
      const after = await screenshot();
      // The first frame of the new page is the first closer to it than to the old one.
      const i = got.findIndex((f) => psnr(f.luma, after) > psnr(f.luma, before));
      if (i >= 0) {
        const page = got.slice(i);
        out.push({
          firstDb: psnr(page[0].luma, after),
          settledDb: psnr(page.at(-1).luma, after),
          firstKB: page[0].bytes / 1000,
          totalKB: page.reduce((a, f) => a + f.bytes, 0) / 1000,
          frames: page.length,
          settleMs: page.at(-1).at - page[0].at,
        });
      }
      before = after;
      await sleep(300);
    }
    view._watchFrames(false);
    await view.showProbe(null);
    return out;
  }, Number(arg('quality')));
  const med = (k) => runs.map((r) => r[k]).sort((a, b) => a - b)[Math.floor(runs.length / 2)] ?? 0;
  result.quality = { runs, median: Object.fromEntries(['firstDb', 'settledDb', 'firstKB', 'totalKB', 'frames', 'settleMs'].map((k) => [k, med(k)])) };
  console.log(`quality: a page change shows at ${med('firstDb').toFixed(1)} dB (${med('firstKB').toFixed(0)} kB), settles at ${med('settledDb').toFixed(1)} dB after ${med('frames') - 1} refinement frame(s) in ${med('settleMs').toFixed(0)} ms, ${med('totalKB').toFixed(0)} kB in all; medians of ${runs.length} changes`);
}

if (Number(arg('connects', 0)) > 0) {
  // Connecting: fresh clients in a page of their own, each with a ticket function the way a host
  // app embeds Skylight (so it may reconnect). For each, the time from connect() to the welcome and
  // to the first decoded frame, then the time from a dropped socket to the first frame on the new
  // connection.
  const p2 = await browser.newPage({ viewport: { width: 1600, height: 900 } });
  const key = loadPrivateKey(keyPath);
  await p2.exposeFunction('benchTicket', () => mintTicket(key, { aud, sub: 'bench', scope: ['view', 'input'] }));
  await p2.goto(base);
  const ws = new URL('ws', base);
  ws.protocol = ws.protocol === 'https:' ? 'wss:' : 'ws:';
  const runs = await p2.evaluate(async ([url, n]) => {
    const { Skylight } = await import(new URL('skylight.js', location.href).href);
    const out = [];
    for (let i = 0; i < n; i++) {
      const el = document.createElement('div');
      el.style.cssText = 'position:fixed;inset:0';
      document.body.append(el);
      const t0 = performance.now();
      const view = new Skylight(el, { url, ticket: () => window.benchTicket() });
      let onFrame = null;
      const sink = view._onSinkEvent.bind(view);
      view._onSinkEvent = (m) => {
        if (m.t === 'frame' && onFrame) {
          onFrame(performance.now());
          onFrame = null;
        }
        sink(m);
      };
      const nextFrame = () => new Promise((r) => (onFrame = r));
      const welcome = new Promise((r) => view.addEventListener('state', (e) => e.detail.state === 'connected' && r(performance.now())));
      await view._videoReady;
      const ready = performance.now();
      let frame = nextFrame();
      view.connect();
      const run = { readyMs: ready - t0, welcomeMs: (await welcome) - ready, firstFrameMs: (await frame) - ready };
      // A dropped connection: the client reconnects on its own.
      await new Promise((r) => setTimeout(r, 1000));
      frame = nextFrame();
      const dropped = performance.now();
      view.ws.close();
      run.reconnectMs = (await frame) - dropped;
      view.disconnect();
      el.remove();
      out.push(run);
      await new Promise((r) => setTimeout(r, 300));
    }
    return out;
  }, [ws.href, Number(arg('connects'))]);
  await p2.close();
  const med = (k) => runs.map((r) => r[k]).sort((a, b) => a - b)[Math.floor(runs.length / 2)];
  const max = (k) => Math.max(...runs.map((r) => r[k]));
  result.connect = { runs, median: Object.fromEntries(['readyMs', 'welcomeMs', 'firstFrameMs', 'reconnectMs'].map((k) => [k, med(k)])) };
  console.log(`connect: welcome ${med('welcomeMs').toFixed(0)} ms, first frame ${med('firstFrameMs').toFixed(0)} ms (max ${max('firstFrameMs').toFixed(0)}), after a drop ${med('reconnectMs').toFixed(0)} ms (max ${max('reconnectMs').toFixed(0)}), video worker ready in ${med('readyMs').toFixed(0)} ms; medians of ${runs.length}`);
}

if (arg('json')) writeFileSync(arg('json'), JSON.stringify(result, null, 2));
await browser.close();
