// The standalone viewer: the Skylight client plus a small toolbar, stats and settings.
// The ticket arrives in the URL fragment (never sent to any server) and is removed at once.

import { Skylight } from './skylight.js';

const $ = (id) => document.getElementById(id);
const params = new URLSearchParams(location.hash.slice(1));
const ticket = params.get('ticket');
history.replaceState(null, '', location.pathname + location.search);

const isTouch = matchMedia('(pointer: coarse)').matches;
const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
document.documentElement.classList.toggle('is-touch', isTouch);
document.documentElement.classList.toggle('is-mac', isMac && !isTouch);

const saved = JSON.parse(localStorage.getItem('skylight') || '{}');
const settings = { cmdKey: 'ctrl', bitrate: '0', codec: 'h264', scale: 'fit', touchMode: 'trackpad', stats: false, ...saved };
const save = () => localStorage.setItem('skylight', JSON.stringify(settings));

const wsUrl = params.get('ws') || (() => {
  const u = new URL('ws', location.href);
  u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
  return u.href;
})();

function status(title, desc, canRetry) {
  $('status').hidden = !title;
  $('status-title').textContent = title || '';
  $('status-desc').textContent = desc || '';
  $('reconnect').hidden = !canRetry;
}

if (!ticket) {
  $('bar').hidden = true;
  status('No ticket', 'Open this page from your dashboard, or mint a link on the server with skylight ticket --url.');
  throw new Error('no ticket');
}

const codecs = [settings.codec, ...['h264', 'hevc', 'av1'].filter((c) => c !== settings.codec)];
const view = new Skylight($('screen'), {
  url: wsUrl,
  ticket,
  codecs,
  cmdKey: settings.cmdKey,
  scale: settings.scale,
  touchMode: settings.touchMode,
  maxBitrate: Number(settings.bitrate) ? Number(settings.bitrate) * 1e6 : undefined,
  reconnect: false,
  // #noworker decodes on the page's thread (for comparisons).
  worker: !params.has('noworker'),
});
window.skylight = view;

view.addEventListener('state', (e) => {
  const { state, detail } = e.detail;
  if (state === 'connecting') status('Connecting', wsUrl.replace(/^wss?:\/\//, ''));
  else if (state === 'connected') {
    status(null);
    view.focus();
  } else if (state === 'disconnected') {
    status('Disconnected', `${detail || 'The connection closed.'} Tickets work once; open a new link to reconnect.`);
  }
});
view.addEventListener('error', (e) => console.warn('skylight:', e.detail));

// Toolbar: out of the way while you work, back when the pointer nears the top edge.
const bar = $('bar');
let barTimer = 0;
const showBar = () => {
  bar.classList.remove('away');
  clearTimeout(barTimer);
  if (!isTouch) {
    barTimer = setTimeout(() => {
      if (!bar.matches(':hover') && $('settings').hidden) bar.classList.add('away');
    }, 2500);
  }
};
addEventListener('pointermove', (e) => {
  if (e.clientY < 56) showBar();
});
showBar();

// Display picker.
view.addEventListener('monitors', (e) => {
  const { monitors, current } = e.detail;
  const multi = monitors.length > 1;
  $('monitorrow').hidden = !multi;
  const seg = $('monitors');
  seg.replaceChildren(...monitors.map((m) => {
    const b = document.createElement('button');
    b.dataset.v = m.id;
    b.textContent = m.id === '*' ? 'All' : m.id;
    b.title = m.name;
    b.setAttribute('aria-pressed', String(m.id === current));
    b.onclick = () => view.setMonitor(m.id);
    return b;
  }));
});

// Segmented settings.
function syncSeg(seg) {
  const key = seg.dataset.setting;
  for (const b of seg.querySelectorAll('button')) b.setAttribute('aria-pressed', String(b.dataset.v === String(settings[key])));
}
for (const seg of document.querySelectorAll('.seg[data-setting]')) {
  if (seg.dataset.setting === 'monitor') continue;
  syncSeg(seg);
  seg.addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    const key = seg.dataset.setting;
    settings[key] = b.dataset.v;
    save();
    syncSeg(seg);
    if (key === 'cmdKey') view.setOptions({ cmdKey: b.dataset.v });
    if (key === 'bitrate') view.setOptions({ maxBitrate: Number(b.dataset.v) ? Number(b.dataset.v) * 1e6 : 1e9 });
    if (key === 'codec') view.setOptions({ codec: b.dataset.v });
  });
}

// Hide codecs this browser cannot decode.
(async () => {
  const probes = { h264: 'avc1.640028', hevc: 'hvc1.1.6.L123.B0', av1: 'av01.0.08M.08' };
  for (const [name, codec] of Object.entries(probes)) {
    let ok = false;
    try {
      ok = (await VideoDecoder.isConfigSupported({ codec, codedWidth: 1920, codedHeight: 1080 })).supported;
    } catch {
      ok = false;
    }
    const b = document.querySelector(`.seg[data-setting="codec"] button[data-v="${name}"]`);
    if (b) b.hidden = !ok;
  }
})();

$('scale').onclick = () => {
  settings.scale = settings.scale === 'fit' ? 'native' : 'fit';
  save();
  view.setOptions({ scale: settings.scale });
  $('scale').setAttribute('aria-label', settings.scale === 'fit' ? 'Fit to window' : 'Actual pixels');
  $('scale').setAttribute('aria-pressed', String(settings.scale === 'native'));
};
$('scale').setAttribute('aria-pressed', String(settings.scale === 'native'));

$('keyboard').onclick = () => view.showKeyboard();
$('touchmode').onclick = () => {
  settings.touchMode = settings.touchMode === 'trackpad' ? 'direct' : 'trackpad';
  save();
  view.setOptions({ touchMode: settings.touchMode });
  $('touchmode').setAttribute('aria-label', `Touch mode: ${settings.touchMode === 'trackpad' ? 'trackpad' : 'direct'}`);
  $('touchmode').setAttribute('aria-pressed', String(settings.touchMode === 'direct'));
};

$('fullscreen').onclick = async () => {
  if (document.fullscreenElement) await document.exitFullscreen();
  else {
    await document.documentElement.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
    // In full screen Chrome can hand us Esc, Cmd+W and friends.
    if (navigator.keyboard && navigator.keyboard.lock) navigator.keyboard.lock().catch(() => {});
  }
  view.focus();
};
if (!document.documentElement.requestFullscreen) $('fullscreen').hidden = true;

// Stats.
const fmt = (v, d = 0) => (Number.isFinite(v) && v > 0 ? v.toFixed(d) : '–');
function toggleStats(on) {
  settings.stats = on;
  save();
  $('stats').hidden = !on;
  $('statsbtn').setAttribute('aria-pressed', String(on));
}
$('statsbtn').onclick = () => toggleStats(!settings.stats);
toggleStats(settings.stats || params.has('stats'));
view.addEventListener('stats', (e) => {
  const s = e.detail;
  $('s-latency').textContent = fmt(s.latencyMs);
  $('s-fps').textContent = `${fmt(s.fps)} fps`;
  $('s-kbps').textContent = s.kbps >= 1000 ? `${(s.kbps / 1000).toFixed(1)} Mbit/s` : `${fmt(s.kbps)} kbit/s`;
  $('s-server').textContent = `${fmt(s.serverMs, 1)} ms`;
  $('s-network').textContent = `${fmt(s.networkMs, 1)} ms`;
  $('s-decode').textContent = `${fmt(s.decodeMs, 1)} ms`;
  $('s-rtt').textContent = `${fmt(s.rttMs, 1)} ms`;
  $('s-stream').textContent = s.width ? `${s.width}×${s.height} ${s.encoder.replace('_', ' ')}` : '–';
});

// Settings popover.
$('settingsbtn').onclick = () => {
  const open = $('settings').hidden;
  $('settings').hidden = !open;
  $('settingsbtn').setAttribute('aria-expanded', String(open));
  if (open) showBar();
  else view.focus();
};

// Latency test: move, click, then type into the probe window.
$('measure').onclick = async () => {
  const btn = $('measure');
  btn.disabled = true;
  btn.textContent = 'Measuring…';
  const out = [];
  try {
    for (const mode of ['click', 'key', 'move']) {
      $('latency-desc').textContent = `Testing ${mode === 'key' ? 'key presses' : mode === 'click' ? 'clicks' : 'pointer moves'}`;
      const r = await view.measureLatency({ mode, samples: 20 });
      out.push(`${mode === 'key' ? 'Key' : mode === 'click' ? 'Click' : 'Move'} ${r.shown.median.toFixed(0)}`);
      console.info('skylight latency', r);
    }
    $('latency-result').textContent = `${out.join(' · ')} ms`;
    $('latency-desc').textContent = 'Median, input sent to frame on screen';
  } catch (e) {
    $('latency-result').textContent = 'Measurement failed';
    $('latency-desc').textContent = String(e.message || e);
  } finally {
    btn.disabled = false;
    btn.textContent = 'Measure';
    view.focus();
  }
};

view.connect();
