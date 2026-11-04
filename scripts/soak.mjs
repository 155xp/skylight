// How well one viewer holds a session over a bad path: a real browser keeps a Skylight open for
// a while, the way a host app embeds it (a ticket function, so it reconnects on its own), with the
// latency probe moving on screen, and counts what the person in front of it would notice.
//
//   node scripts/soak.mjs [--base http://127.0.0.1:5491/] [--seconds 120] [--mode scroll]
//                         [--key $TESTBED_DIR/ticket-private.pem] [--aud testbed] [--json out.json]
//
// Run it over a path made bad with tc netem (scripts/netns-soak.sh) or scripts/netem.mjs.
// Reports drops (a connected view going to disconnected), why, how long each took to show a frame
// again, the share of the time a picture was live, frames per second and the longest freeze.
import { chromium } from 'playwright-core';
import { writeFileSync } from 'node:fs';
import { loadPrivateKey, mintTicket } from '../clients/node/mint-ticket.mjs';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : def;
};
const base = arg('base', 'http://127.0.0.1:5491/');
const seconds = Number(arg('seconds', 120));
const mode = arg('mode', 'scroll');
const key = loadPrivateKey(arg('key', `${process.env.TESTBED_DIR || '/tmp/skylight-testbed'}/ticket-private.pem`));
const aud = arg('aud', 'testbed');

const browser = await chromium.launch({
  executablePath: arg('chromium', '/usr/bin/chromium'),
  args: ['--autoplay-policy=no-user-gesture-required', '--no-sandbox'],
});
const page = await browser.newPage({ viewport: { width: 1600, height: 900 } });
page.on('console', (m) => {
  if (m.type() === 'error') console.error('[page]', m.text());
});
await page.exposeFunction('soakTicket', () => mintTicket(key, { aud, sub: 'soak', scope: ['view', 'input'] }));
await page.goto(base);
const ws = new URL('ws', base);
ws.protocol = ws.protocol === 'https:' ? 'wss:' : 'ws:';

const r = await page.evaluate(async ([url, seconds, mode]) => {
  const { Skylight } = await import(new URL('skylight.js', location.href).href);
  const el = document.createElement('div');
  el.style.cssText = 'position:fixed;inset:0';
  document.body.append(el);
  const view = new Skylight(el, { url, ticket: () => window.soakTicket() });
  const out = { drops: [], welcomes: 0, frames: 0, longestFreezeMs: 0, rtt: [], series: [] };
  let lastFrame = 0;
  let droppedAt = 0;
  let live = 0; // ms with a frame shown in the last second
  const sink = view._onSinkEvent.bind(view);
  view._onSinkEvent = (m) => {
    if (m.t === 'frame') {
      const now = performance.now();
      if (lastFrame) out.longestFreezeMs = Math.max(out.longestFreezeMs, now - lastFrame);
      lastFrame = now;
      out.frames++;
      if (droppedAt) {
        out.drops[out.drops.length - 1].backMs = Math.round(now - droppedAt);
        droppedAt = 0;
      }
    }
    sink(m);
  };
  let connected = false;
  let done = false;
  let stalled = false;
  out.stalls = 0;
  out.recovered = 0;
  view.addEventListener('state', (e) => {
    const { state, detail } = e.detail;
    if (state === 'reconnecting') {
      stalled = true;
      out.stalls++;
    } else if (state === 'connected' && stalled) {
      stalled = false; // the same connection, back after a stall
      out.recovered++;
    } else if (state === 'connected') {
      out.welcomes++;
      connected = true;
      // Keep the picture moving: the probe lives as long as its session, so start it each time.
      view._send({ t: 'bench', action: 'start', mode });
    } else if (state === 'disconnected' && connected && !done) {
      connected = false;
      stalled = false;
      out.drops.push({ atS: Math.round((performance.now() - t0) / 100) / 10, why: String(detail || '') });
      droppedAt = performance.now();
    }
  });
  view.addEventListener('stats', (e) => {
    if (e.detail.rttMs) out.rtt.push(e.detail.rttMs);
    const s = view.serverStats || {};
    out.series.push({ t: Math.round((performance.now() - t0) / 1000), fps: +e.detail.fps.toFixed(1), kbps: Math.round(e.detail.kbps), rtt: Math.round(e.detail.rttMs), target: s.targetKbps, srvRtt: Math.round(s.rttMs || 0), minRtt: Math.round(s.minRttMs || 0), stalls: s.stalls, inflight: s.inflight });
  });
  await view._videoReady;
  const t0 = performance.now();
  view.connect();
  const end = t0 + seconds * 1000;
  while (performance.now() < end) {
    await new Promise((r) => setTimeout(r, 250));
    if (lastFrame && performance.now() - lastFrame < 1000) live += 250;
  }
  if (lastFrame) out.longestFreezeMs = Math.max(out.longestFreezeMs, performance.now() - lastFrame);
  out.liveShare = live / (seconds * 1000);
  out.fps = out.frames / seconds;
  done = true;
  view.disconnect();
  return out;
}, [ws.href, seconds, mode]);
await browser.close();

const rtt = r.rtt.sort((a, b) => a - b);
const backs = r.drops.map((d) => d.backMs).filter((x) => x != null).sort((a, b) => a - b);
const result = {
  seconds,
  drops: r.drops.length,
  dropsPerMin: +(r.drops.length / (seconds / 60)).toFixed(2),
  sessions: r.welcomes,
  stalls: r.stalls,
  stallsRecovered: r.recovered,
  livePct: +(100 * r.liveShare).toFixed(1),
  fps: +r.fps.toFixed(1),
  longestFreezeMs: Math.round(r.longestFreezeMs),
  medianBackMs: backs.length ? backs[Math.floor(backs.length / 2)] : null,
  rttMedianMs: rtt.length ? Math.round(rtt[Math.floor(rtt.length / 2)]) : null,
  why: Object.entries(r.drops.reduce((a, d) => ((a[d.why] = (a[d.why] || 0) + 1), a), {})),
};
console.log(JSON.stringify(result));
if (arg('json')) writeFileSync(arg('json'), JSON.stringify({ ...result, detail: r.drops, series: r.series }, null, 2));
