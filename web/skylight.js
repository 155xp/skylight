// Skylight browser client. One ES module, no dependencies.
//
//   import { Skylight } from './skylight.js';
//   const view = new Skylight(element, {
//     url: 'wss://desk.example.com/ws',          // or a list, tried in order
//     ticket: async () => (await fetch('/api/desk/ticket', { method: 'POST' })).text(),
//   });
//   view.addEventListener('state', (e) => console.log(e.detail));
//   await view.connect();
//
// Video arrives as H.264, HEVC or AV1 access units over a WebSocket and goes straight into a
// WebCodecs VideoDecoder (hardware where the browser has it), then onto a canvas the moment it
// is decoded. Every decoded frame is acknowledged so the server can pace itself. All of that runs
// in a worker (built from this file, so there is still only one file to serve), so nothing the
// page does can hold a frame back.

const MSG_VIDEO = 1;
const MSG_CURSOR_SHAPE = 2;
const MSG_CURSOR_POS = 3;
const IN_POINTER_ABS = 0x10;
const IN_POINTER_REL = 0x11;
const IN_BUTTON = 0x12;
const IN_SCROLL = 0x13;
const IN_KEY = 0x14;
const IN_TOUCH = 0x15;
const IN_ACK = 0x16;
const IN_TEXT = 0x17;
const FLAG_KEY = 1;
const FLAG_REFINE = 2;
const CODECS = { 1: 'h264', 2: 'hevc', 3: 'av1' };

// KeyboardEvent.code to Linux evdev keycodes.
const KEYMAP = {
  Escape: 1, Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6, Digit6: 7, Digit7: 8, Digit8: 9,
  Digit9: 10, Digit0: 11, Minus: 12, Equal: 13, Backspace: 14, Tab: 15, KeyQ: 16, KeyW: 17, KeyE: 18,
  KeyR: 19, KeyT: 20, KeyY: 21, KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25, BracketLeft: 26,
  BracketRight: 27, Enter: 28, ControlLeft: 29, KeyA: 30, KeyS: 31, KeyD: 32, KeyF: 33, KeyG: 34,
  KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38, Semicolon: 39, Quote: 40, Backquote: 41, ShiftLeft: 42,
  Backslash: 43, KeyZ: 44, KeyX: 45, KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50, Comma: 51,
  Period: 52, Slash: 53, ShiftRight: 54, NumpadMultiply: 55, AltLeft: 56, Space: 57, CapsLock: 58,
  F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66, F9: 67, F10: 68, NumLock: 69,
  ScrollLock: 70, Numpad7: 71, Numpad8: 72, Numpad9: 73, NumpadSubtract: 74, Numpad4: 75,
  Numpad5: 76, Numpad6: 77, NumpadAdd: 78, Numpad1: 79, Numpad2: 80, Numpad3: 81, Numpad0: 82,
  NumpadDecimal: 83, IntlBackslash: 86, F11: 87, F12: 88, IntlRo: 89, NumpadEnter: 96,
  ControlRight: 97, NumpadDivide: 98, PrintScreen: 99, AltRight: 100, Home: 102, ArrowUp: 103,
  PageUp: 104, ArrowLeft: 105, ArrowRight: 106, End: 107, ArrowDown: 108, PageDown: 109,
  Insert: 110, Delete: 111, AudioVolumeMute: 113, AudioVolumeDown: 114, AudioVolumeUp: 115,
  NumpadEqual: 117, Pause: 119, NumpadComma: 121, Lang1: 122, Lang2: 123, IntlYen: 124,
  MetaLeft: 125, MetaRight: 126, ContextMenu: 127, MediaTrackNext: 163, MediaPlayPause: 164,
  MediaTrackPrevious: 165, MediaStop: 166, F13: 183, F14: 184, F15: 185, F16: 186, F17: 187,
  F18: 188, F19: 189, F20: 190, F21: 191, F22: 192, F23: 193, F24: 194,
};
const KEY_LEFTCTRL = 29;
const KEY_RIGHTCTRL = 97;
const KEY_BACKSPACE = 14;
const KEY_ENTER = 28;
const MODIFIERS = new Set([29, 97, 42, 54, 56, 100, 125, 126]);

const isApple = /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);

function median(a) {
  if (!a.length) return 0;
  const s = [...a].sort((x, y) => x - y);
  return s[Math.floor(s.length / 2)];
}

function percentile(a, p) {
  if (!a.length) return 0;
  const s = [...a].sort((x, y) => x - y);
  return s[Math.min(s.length - 1, Math.round((s.length - 1) * p))];
}

// --- The video path ----------------------------------------------------------------------------

/**
 * Everything video needs, in one self-contained function: it refers to nothing outside itself but
 * browser globals. The page calls it, and so does the worker, which gets its source as text
 * (`videoWorkerSource`), so bundlers that rename or inline module-level names cannot break the
 * worker.
 */
function videoCore() {
  const MSG_VIDEO = 1;
  const IN_ACK = 0x16;
  const FLAG_KEY = 1;
  const FLAG_REFINE = 2;
  const CODECS = { 1: 'h264', 2: 'hevc', 3: 'av1' };

  // --- Bitstream helpers ------------------------------------------------------------------------

  /** Split an Annex B access unit into NAL units (without start codes). */
  function splitAnnexB(buf) {
    const out = [];
    let i = 0;
    let start = -1;
    const n = buf.length;
    while (i + 2 < n) {
      if (buf[i] === 0 && buf[i + 1] === 0 && (buf[i + 2] === 1 || (buf[i + 2] === 0 && i + 3 < n && buf[i + 3] === 1))) {
        const sc = buf[i + 2] === 1 ? 3 : 4;
        if (start >= 0) out.push(buf.subarray(start, i));
        i += sc;
        start = i;
      } else {
        i++;
      }
    }
    if (start >= 0) out.push(buf.subarray(start));
    return out;
  }

  function hex2(v) {
    return v.toString(16).padStart(2, '0');
  }

  /** H.264: build the avcC record and the `avc1.PPCCLL` codec string from SPS and PPS. */
  function avcConfig(sps, pps) {
    const rec = new Uint8Array(11 + sps.length + pps.length + 4);
    let o = 0;
    rec[o++] = 1;
    rec[o++] = sps[1];
    rec[o++] = sps[2];
    rec[o++] = sps[3];
    rec[o++] = 0xff; // 4-byte NAL lengths
    rec[o++] = 0xe1; // one SPS
    rec[o++] = sps.length >> 8;
    rec[o++] = sps.length & 0xff;
    rec.set(sps, o);
    o += sps.length;
    rec[o++] = 1; // one PPS
    rec[o++] = pps.length >> 8;
    rec[o++] = pps.length & 0xff;
    rec.set(pps, o);
    o += pps.length;
    // High-profile extension: 4:2:0, 8-bit, no SPS extensions.
    rec[o++] = 0xfd;
    rec[o++] = 0xf8;
    rec[o++] = 0xf8;
    rec[o++] = 0;
    const high = [100, 110, 122, 244, 44, 83, 86, 118, 128].includes(sps[1]);
    return {
      codec: `avc1.${hex2(sps[1])}${hex2(sps[2])}${hex2(sps[3])}`,
      description: high ? rec : rec.subarray(0, o - 4),
    };
  }

  /** Annex B access unit to length-prefixed (AVCC) samples, dropping parameter sets and AUDs. */
  function toAvcc(nals) {
    let len = 0;
    const keep = nals.filter((n) => {
      const t = n[0] & 0x1f;
      return t !== 7 && t !== 8 && t !== 9;
    });
    for (const n of keep) len += 4 + n.length;
    const out = new Uint8Array(len);
    let o = 0;
    for (const n of keep) {
      out[o++] = n.length >>> 24;
      out[o++] = (n.length >>> 16) & 0xff;
      out[o++] = (n.length >>> 8) & 0xff;
      out[o++] = n.length & 0xff;
      out.set(n, o);
      o += n.length;
    }
    return out;
  }

  function bytesEqual(a, b) {
    if (!a || !b || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
  }

  /**
   * Parse, decode, draw and acknowledge video. Runs inside a worker that owns the video socket and
   * an OffscreenCanvas wherever the browser allows it, so a busy page (layout, charts, garbage
   * collection) never delays a frame; otherwise on the page's thread. Times it reports are
   * absolute (timeOrigin + now), since a worker's clock starts at a different moment.
   */
  class VideoSink {
    constructor(canvas, hooks) {
      this.canvas = canvas;
      this.ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
      this.hooks = hooks;
      this.decoder = null;
      this.config = null;
      this.sps = null;
      this.pps = null;
      this.pending = new Map();
      this.askedKey = 0;
      this.watch = false;
      // Watchdog state, see check().
      this.software = false;
      this.needKey = false;
      this.shownAt = 0;
      this.stalls = 0;
      this.black = 0;
      this.probe = null;
    }

    static now() {
      return performance.timeOrigin + performance.now();
    }

    onVideo(buf) {
      const recv = VideoSink.now();
      const b = new Uint8Array(buf);
      const dv = new DataView(buf);
      const flags = b[1];
      const codec = CODECS[b[2]];
      const id = dv.getUint32(4, true);
      const width = dv.getUint16(8, true);
      const height = dv.getUint16(10, true);
      const data = b.subarray(28);
      const key = (flags & FLAG_KEY) !== 0;
      if (width !== this.canvas.width || height !== this.canvas.height) {
        this.canvas.width = width;
        this.canvas.height = height;
        this.hooks.event({ t: 'size', width, height });
      }
      let chunkData = data;
      if (codec === 'h264') {
        const nals = splitAnnexB(data);
        if (key) {
          const sps = nals.find((n) => (n[0] & 0x1f) === 7);
          const pps = nals.find((n) => (n[0] & 0x1f) === 8);
          if (sps && pps && (!this.config || !bytesEqual(this.sps, sps) || !bytesEqual(this.pps, pps))) {
            this.sps = sps;
            this.pps = pps;
            const { codec: cs, description } = avcConfig(sps, pps);
            this.configure({ codec: cs, description, codedWidth: width, codedHeight: height });
          }
        }
        chunkData = toAvcc(nals);
      } else if (key && (!this.config || this.config._for !== codec)) {
        const cs = codec === 'hevc' ? 'hvc1.1.6.L123.B0' : 'av01.0.08M.08';
        this.configure({ codec: cs, codedWidth: width, codedHeight: height, _for: codec });
      }
      if (!this.decoder || this.decoder.state !== 'configured' || (this.needKey && !key)) {
        if (recv - this.askedKey > 500) {
          this.askedKey = recv;
          this.hooks.send(JSON.stringify({ t: 'keyframe' }));
        }
        return;
      }
      if (key) this.needKey = false;
      this.pending.set(id, {
        recv,
        captureUs: Number(dv.getBigUint64(12, true)),
        queueUs: dv.getUint32(20, true),
        encodeUs: dv.getUint32(24, true),
        refine: (flags & FLAG_REFINE) !== 0,
        key,
        bytes: b.length,
      });
      try {
        this.decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: id, data: chunkData }));
      } catch (e) {
        this.failed(e);
      }
    }

    configure(cfg) {
      const { _for, ...config } = cfg;
      if (!this.decoder || this.decoder.state === 'closed') {
        this.decoder = new VideoDecoder({ output: (f) => this.onFrame(f), error: (e) => this.failed(e) });
      }
      try {
        this.decoder.configure({
          ...config,
          optimizeForLatency: true,
          hardwareAcceleration: this.software ? 'prefer-software' : 'no-preference',
        });
        this.config = { ...config, _for: _for || 'h264' };
        this.hooks.event({ t: 'config', codec: config.codec });
      } catch (e) {
        this.failed(e);
      }
    }

    failed(e) {
      this.hooks.event({ t: 'error', message: `decoder: ${e.message || e}` });
      this.close();
      this.hooks.send(JSON.stringify({ t: 'keyframe' }));
    }

    onFrame(frame) {
      const now = VideoSink.now();
      const id = frame.timestamp;
      const meta = this.pending.get(id);
      this.pending.delete(id);
      // Anything older than this frame was dropped by the decoder.
      for (const k of this.pending.keys()) if (k < id) this.pending.delete(k);
      // Paint on arrival: no waiting for an animation frame, the newest frame simply wins.
      this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
      this.shownAt = now;
      if (meta) {
        const ack = new ArrayBuffer(12);
        const dv = new DataView(ack);
        dv.setUint8(0, IN_ACK);
        dv.setUint32(4, id, true);
        dv.setUint32(8, Math.round((now - meta.recv) * 1000), true);
        this.hooks.send(ack);
      }
      if (this.watch) {
        // For latency measurements, which read pixels out of the decoded frame.
        const copy = frame.clone();
        this.hooks.event({ t: 'vframe', frame: copy, decoded: now, meta }, [copy]);
      }
      frame.close();
      if (meta) this.hooks.event({ t: 'frame', decoded: now, ...meta });
    }

    /** A new stream (codec, size or monitor) starts with its own keyframe. */
    reset() {
      this.config = null;
      this.shownAt = 0;
    }

    /**
     * The watchdog, called once a second while connected: check that what the server sends reaches
     * the screen. A decoder that sits on frames is flushed; one that keeps doing it, or that draws
     * black while the server's picture (`luma`, its brightest sampled pixel) is not, is swapped for
     * a software decoder. Reports `health` events; `connectedMs` is the time since connecting.
     */
    check(luma, connectedMs) {
      const now = VideoSink.now();
      // `final`: nothing left to try here, so the host page may want another transport.
      const health = (issue, detail, final = false) => this.hooks.event({ t: 'health', issue, detail, final });
      let oldest = Infinity;
      for (const p of this.pending.values()) oldest = Math.min(oldest, p.recv);
      if (this.decoder?.state === 'configured' && now - oldest > 1500 && this.shownAt < oldest) {
        if (++this.stalls >= 2 && !this.software) {
          this.useSoftware();
          health('decoder-stall', 'the video decoder kept holding frames back; switched to the software decoder');
        } else {
          health('decoder-stall', `the video decoder held ${this.pending.size} frame(s) back for over 1.5 s`);
          // flush() hands the held frames out; the decoder then needs a keyframe.
          this.pending.clear();
          this.needKey = true;
          this.decoder.flush().then(() => this.hooks.send(JSON.stringify({ t: 'keyframe' })), () => {});
        }
        return;
      }
      if (!this.shownAt) {
        if (connectedMs > 3000) this.hooks.send(JSON.stringify({ t: 'keyframe' }));
        if (connectedMs > 8000) health('no-video', 'connected, but no picture has arrived', true);
        return;
      }
      if (luma >= 48 && now - this.shownAt < 30000 && this.lumaMax() < 8) {
        if (++this.black >= 3) {
          this.black = 0;
          if (!this.software) {
            this.useSoftware();
            health('black-output', 'the video decoder draws a black picture; switched to the software decoder');
          } else {
            health('black-output', 'the picture stays black even with the software decoder', true);
          }
        }
        return;
      }
      this.black = 0;
      health(null, '');
    }

    lumaMax() {
      if (!this.probe) {
        const c = typeof OffscreenCanvas !== 'undefined' ? new OffscreenCanvas(32, 18) : Object.assign(document.createElement('canvas'), { width: 32, height: 18 });
        this.probe = c.getContext('2d', { willReadFrequently: true });
      }
      this.probe.drawImage(this.canvas, 0, 0, 32, 18);
      const d = this.probe.getImageData(0, 0, 32, 18).data;
      let max = 0;
      for (let i = 0; i < d.length; i += 4) max = Math.max(max, (d[i] * 54 + d[i + 1] * 183 + d[i + 2] * 19) >> 8);
      return max;
    }

    useSoftware() {
      this.software = true;
      this.close();
      this.hooks.send(JSON.stringify({ t: 'keyframe' }));
    }

    close() {
      if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
      this.decoder = null;
      this.config = null;
      this.pending.clear();
    }
  }

  return { MSG_VIDEO, splitAnnexB, avcConfig, VideoSink };
}

const core = videoCore();
const VideoSink = core.VideoSink;
/** Split an Annex B access unit into NAL units (without start codes). */
export const splitAnnexB = core.splitAnnexB;
/** H.264: build the avcC record and the `avc1.PPCCLL` codec string from SPS and PPS. */
export const avcConfig = core.avcConfig;

/** The worker: owns the main WebSocket and a VideoSink; everything else goes to the page. */
function videoWorker(core) {
  let canvas = null;
  let ws = null;
  let sink = null;
  let watch = false;
  let software = false;
  self.onmessage = (e) => {
    const m = e.data;
    if (m.t === 'canvas') {
      canvas = m.canvas;
    } else if (m.t === 'open') {
      const id = m.id;
      const sock = new WebSocket(m.url);
      sock.binaryType = 'arraybuffer';
      if (sink) sink.close();
      ws = sock;
      sink = new core.VideoSink(canvas, {
        send: (d) => sock.readyState === 1 && sock.send(d),
        event: (ev, transfer) => self.postMessage({ ...ev, id }, transfer || []),
      });
      sink.watch = watch;
      sink.software = software;
      const own = sink;
      sock.onopen = () => self.postMessage({ t: 'open', id });
      sock.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          if (ev.data.includes('"stream"')) {
            try {
              if (JSON.parse(ev.data).t === 'stream') own.reset();
            } catch {}
          }
          self.postMessage({ t: 'text', id, data: ev.data });
        } else if (new Uint8Array(ev.data, 0, 1)[0] === core.MSG_VIDEO) {
          own.onVideo(ev.data);
        } else {
          self.postMessage({ t: 'binary', id, data: ev.data }, [ev.data]);
        }
      };
      sock.onclose = () => {
        own.close();
        if (ws === sock) ws = null;
        self.postMessage({ t: 'close', id });
      };
    } else if (m.t === 'send') {
      if (ws && ws.readyState === 1) ws.send(m.data);
    } else if (m.t === 'close') {
      if (ws) ws.close();
    } else if (m.t === 'watch') {
      watch = m.on;
      if (sink) sink.watch = m.on;
    } else if (m.t === 'check') {
      if (sink) {
        sink.check(m.luma, m.connectedMs);
        software = sink.software;
      }
    }
  };
  self.postMessage({ t: typeof VideoDecoder === 'undefined' || typeof OffscreenCanvas === 'undefined' ? 'unsupported' : 'ready' });
}

/**
 * Source for the worker, built from this module's own code so embedding stays one file. Both
 * functions are self-contained, so this survives minification.
 */
function videoWorkerSource() {
  return `(${videoWorker})((${videoCore})());`;
}

/** What `_open` needs of a WebSocket, for a socket that lives in the video worker. */
class WorkerSocket {
  constructor(worker, url, id) {
    this.worker = worker;
    this.id = id;
    this.readyState = 0;
    this.binaryType = 'arraybuffer';
    worker.postMessage({ t: 'open', url, id });
  }

  send(data) {
    if (this.readyState === 1) this.worker.postMessage({ t: 'send', data });
  }

  close() {
    if (this.readyState < 2) {
      this.readyState = 2;
      this.worker.postMessage({ t: 'close' });
    }
  }
}

// --- The client ---------------------------------------------------------------------------------

export class Skylight extends EventTarget {
  /**
   * @param {HTMLElement} container
   * @param {{url: string|string[], ticket: string|(() => Promise<string>|string), maxFps?: number,
   *   maxBitrate?: number, codecs?: string[], cmdKey?: 'ctrl'|'super', touchMode?: 'trackpad'|'direct',
   *   scale?: 'fit'|'native', viewOnly?: boolean, clipboard?: boolean, reconnect?: boolean,
   *   monitor?: string}} options
   */
  constructor(container, options) {
    super();
    this.container = container;
    this.opts = {
      codecs: ['h264', 'hevc', 'av1'],
      cmdKey: 'ctrl',
      touchMode: 'trackpad',
      scale: 'fit',
      viewOnly: false,
      clipboard: true,
      reconnect: true,
      ...options,
    };
    this.state = 'idle';
    this.monitors = [];
    this.monitor = null;
    this.welcome = null;
    this.serverStats = {};
    this.stats = {};
    this.ws = null;
    this.decoderConfig = null;
    this.streamInfo = null;
    this.width = 0;
    this.height = 0;
    this.cursor = { x: 0, y: 0, visible: true, url: null, hotX: 0, hotY: 0, w: 0, h: 0, rgba: null };
    this.pressedKeys = new Set();
    this.clock = { offset: 0, bestRtt: Infinity, samples: [] };
    /** Everything that arrived on the main socket, for idle-cost checks. */
    this.received = { messages: 0, bytes: 0 };
    this.win = this._newWindow();
    this.view = { scale: 1, tx: 0, ty: 0 };
    this.vcursor = { x: 0, y: 0 };
    this.bench = null;
    this.retry = 0;
    this._healthIssue = null;
    this._build();
    this._bindInput();
    this._measureRefresh();
  }

  // ---------------------------------------------------------------- public API

  async connect() {
    this._closing = false;
    await this._videoReady;
    const urls = Array.isArray(this.opts.url) ? this.opts.url : [this.opts.url];
    this._setState('connecting');
    let lastErr = null;
    let ticket = null;
    for (const url of urls) {
      // A ticket is only sent once the socket is open, so a connection that dies during the
      // TCP or TLS handshake (some tunnels reset a share of new connections) can be retried
      // with the same ticket.
      for (let attempt = 0; attempt < 3; attempt++) {
        try {
          if (!ticket) ticket = typeof this.opts.ticket === 'function' ? await this.opts.ticket() : this.opts.ticket;
          await this._open(url, ticket);
          return;
        } catch (e) {
          lastErr = e;
          if (e.ticketSpent) ticket = null;
          if (e.ticketSpent && typeof this.opts.ticket !== 'function') break;
          if (!e.beforeOpen) break;
        }
      }
    }
    this._setState('disconnected', lastErr ? String(lastErr.message || lastErr) : 'could not connect');
    this._scheduleReconnect();
  }

  disconnect() {
    this._closing = true;
    clearTimeout(this._reconnectTimer);
    if (this.ws) this.ws.close();
    this._teardown();
    this._setState('disconnected', 'closed');
  }

  setMonitor(id) {
    this._send({ t: 'monitor', id });
  }

  setOptions(o) {
    Object.assign(this.opts, o);
    if ('maxFps' in o || 'maxBitrate' in o || 'codec' in o) {
      this._send({ t: 'settings', maxFps: o.maxFps, maxBitrate: o.maxBitrate, codec: o.codec });
    }
    if ('scale' in o) this._layout();
    if ('touchMode' in o) this._updateCursorMode();
  }

  requestKeyframe() {
    this._send({ t: 'keyframe' });
  }

  focus() {
    this.ime.focus({ preventScroll: true });
  }

  /** Show the on-screen keyboard on phones. Call from a tap handler. */
  showKeyboard() {
    this.ime.focus({ preventScroll: true });
  }

  /** Type text on the remote desktop (US layout). */
  typeText(text) {
    if (!text) return;
    const b = new TextEncoder().encode(text);
    const m = new Uint8Array(1 + b.length);
    m[0] = IN_TEXT;
    m.set(b, 1);
    this._sendInput(m);
  }

  /** Press and release a combination of evdev keycodes, e.g. [29, 56, 111] for Ctrl+Alt+Del. */
  sendKeys(codes) {
    for (const c of codes) this._key(c, true);
    for (const c of [...codes].reverse()) this._key(c, false);
  }

  /**
   * Input-to-photon latency. The server opens a small probe window on the desktop that redraws
   * in response to input; this triggers the input and times the change in the decoded video.
   *   mode 'move':  pointer moves across it (white/black halves)
   *   mode 'click': left clicks flip it between white and black
   *   mode 'key':   key presses flip it between a blank page and a page of glyphs
   * Returns times from sending the input to the first decoded frame that shows the change
   * (`decoded`) and to the next display refresh after it was drawn (`shown`).
   */
  async measureLatency({ mode = 'click', samples = 30 } = {}) {
    if (this.bench) throw new Error('a measurement is already running');
    const probe = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('the server did not start the probe')), 5000);
      this._benchReply = (m) => {
        clearTimeout(timer);
        if (m.probe) resolve(m.probe);
        else reject(new Error(m.error || 'probe failed'));
      };
      this._send({ t: 'bench', action: 'start', mode });
    });
    const [px, py, pw, ph] = probe;
    this._benchWatch = { probe };
    this._watchFrames(true);
    this._rafTimes = [];
    const tick = (t) => {
      if (!this._benchWatch) return;
      this._rafTimes.push(t);
      if (this._rafTimes.length > 240) this._rafTimes.shift();
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    const cx = px + pw / 2;
    const cy = py + ph / 2;
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    let step = 0;
    const act = () => {
      step++;
      if (mode === 'move') this._pointer(px + pw * (step % 2 ? 0.75 : 0.25), cy);
      else if (mode === 'click') {
        this._button(0, true);
        this._button(0, false);
      } else {
        this._key(KEYMAP.KeyK, true);
        this._key(KEYMAP.KeyK, false);
      }
    };
    const decoded = [];
    const shown = [];
    let failed = 0;
    try {
      await sleep(400);
      this._pointer(mode === 'move' ? px + pw * 0.25 : cx, cy);
      await sleep(150);
      if (mode === 'key') {
        // Focus the probe window so it gets the keys.
        this._button(0, true);
        this._button(0, false);
        await sleep(300);
      }
      // Learn what the two states look like.
      const levels = [];
      for (let i = 0; i < 4; i++) {
        act();
        await sleep(350);
        levels.push(this._probeLevel);
      }
      const a = (levels[1] + levels[3]) / 2;
      const b = (levels[0] + levels[2]) / 2;
      if (!(Math.abs(a - b) > 20)) throw new Error('the probe window is not visible in the stream');
      const threshold = (a + b) / 2;
      for (let i = 0; i < samples; i++) {
        const wantHigh = step % 2 === 0 ? b > a : a > b;
        try {
          const r = await this._benchOnce(act, probe, threshold, wantHigh, 1500);
          decoded.push(r.decoded);
          shown.push(r.shown);
        } catch {
          failed++;
          if (failed > 5) throw new Error('the probe stopped responding');
        }
        this.dispatchEvent(new CustomEvent('bench', { detail: { mode, done: i + 1, samples } }));
        await sleep(90 + Math.random() * 110);
      }
    } finally {
      this.bench = null;
      this._benchWatch = null;
      this._watchFrames(false);
      this._send({ t: 'bench', action: 'stop' });
    }
    const sum = (v) => ({
      median: median(v),
      p95: percentile(v, 0.95),
      min: Math.min(...v),
      max: Math.max(...v),
      mean: v.reduce((x, y) => x + y, 0) / v.length,
    });
    return { mode, samples: decoded.length, failed, decoded: sum(decoded), shown: sum(shown) };
  }

  /**
   * Glass-to-glass latency under motion. The animated probe stamps every redraw with the server
   * clock in a strip of black and white cells; this reads the strip out of each decoded frame and
   * times it against the local clock (through the ping clock offset). Returns the time from the
   * app drawing a frame to it being decoded here and to the next display refresh, and how many
   * distinct redraws arrived per second.
   */
  async measureGlass({ seconds = 10, mode = 'animate' } = {}) {
    const probe = await this.showProbe(mode);
    const tc = { probe, decoded: [], shown: [], codes: new Set(), bad: 0, start: 0 };
    await new Promise((r) => setTimeout(r, 1000));
    tc.start = performance.now();
    this._timecode = tc;
    this._watchFrames(true);
    await new Promise((r) => setTimeout(r, seconds * 1000));
    this._timecode = null;
    this._watchFrames(false);
    const secs = (performance.now() - tc.start) / 1000;
    await this.showProbe(null);
    const sum = (v) => ({ median: median(v), p95: percentile(v, 0.95), min: Math.min(...v), max: Math.max(...v) });
    return { samples: tc.decoded.length, uniqueFps: tc.codes.size / secs, rejected: tc.bad, decoded: sum(tc.decoded), shown: sum(tc.shown) };
  }

  _timecodeCheck(frame, now) {
    const tc = this._timecode;
    const [x, y] = tc.probe;
    const rect = { x: x & ~1, y: y & ~1, width: (32 * 14 + 2) & ~1, height: 48 };
    let copy;
    try {
      copy = frame.clone();
    } catch {
      return;
    }
    const buf = new Uint8Array(copy.allocationSize({ rect }));
    const job = (this._tcChain || Promise.resolve()).then(async () => {
      try {
        const layout = await copy.copyTo(buf, { rect });
        const { offset, stride } = layout[0];
        const rgb = !(copy.format || '').match(/^(I4|NV12)/);
        const luma = (cx, cy) => {
          const i = offset + cy * stride + cx * (rgb ? 4 : 1);
          return rgb ? (buf[i] + buf[i + 1] + buf[i + 2]) / 3 : buf[i];
        };
        let code = 0;
        for (let bit = 0; bit < 32; bit++) {
          const cx = (x - rect.x) + bit * 14 + 7;
          const top = luma(cx, (y - rect.y) + 12);
          const bottom = luma(cx, (y - rect.y) + 36);
          if (Math.abs(top - bottom) < 60) {
            tc.bad++;
            return;
          }
          code = code * 2 + (top < bottom ? 1 : 0);
        }
        if (this._timecode !== tc || tc.codes.has(code)) return;
        tc.codes.add(code);
        // 100 µs units of the server clock, 32 bits; unwrap against the frame's arrival.
        const nowServerUs = (now + this.clock.offset) * 1000;
        const span = 2 ** 32 * 100;
        let drawnUs = code * 100;
        drawnUs += Math.round((nowServerUs - drawnUs) / span) * span;
        const drawn = drawnUs / 1000 - this.clock.offset;
        tc.decoded.push(now - drawn);
        requestAnimationFrame((t) => tc.shown.push(Math.max(t, now) - drawn));
      } finally {
        copy.close();
      }
    });
    this._tcChain = job.catch(() => {});
  }

  /** Open the probe window in a mode (e.g. 'animate' for a moving test pattern); null closes it. */
  showProbe(mode) {
    if (!mode) {
      this._send({ t: 'bench', action: 'stop' });
      return Promise.resolve(null);
    }
    return new Promise((resolve, reject) => {
      this._benchReply = (m) => (m.probe ? resolve(m.probe) : reject(new Error(m.error || 'probe failed')));
      this._send({ t: 'bench', action: 'start', mode });
    });
  }

  // ---------------------------------------------------------------- DOM

  _build() {
    const root = document.createElement('div');
    const s = root.style;
    s.position = 'relative';
    s.overflow = 'hidden';
    s.width = '100%';
    s.height = '100%';
    s.background = '#000';
    s.touchAction = 'none';
    s.userSelect = 'none';
    s.webkitUserSelect = 'none';
    s.webkitTouchCallout = 'none';
    s.outline = 'none';
    const stage = document.createElement('div');
    stage.style.position = 'absolute';
    stage.style.left = '0';
    stage.style.top = '0';
    stage.style.transformOrigin = '0 0';
    const canvas = document.createElement('canvas');
    canvas.width = 16;
    canvas.height = 9;
    canvas.style.position = 'absolute';
    canvas.style.left = '0';
    canvas.style.top = '0';
    canvas.style.display = 'block';
    const cursor = document.createElement('div');
    cursor.style.position = 'absolute';
    cursor.style.left = '0';
    cursor.style.top = '0';
    cursor.style.pointerEvents = 'none';
    cursor.style.display = 'none';
    cursor.style.backgroundRepeat = 'no-repeat';
    cursor.style.backgroundSize = '100% 100%';
    cursor.style.willChange = 'transform';
    // A hidden text field takes keyboard focus: it lets paste events and phone keyboards work.
    const ime = document.createElement('textarea');
    ime.setAttribute('autocapitalize', 'off');
    ime.setAttribute('autocomplete', 'off');
    ime.setAttribute('autocorrect', 'off');
    ime.setAttribute('spellcheck', 'false');
    ime.setAttribute('aria-label', 'Remote keyboard input');
    Object.assign(ime.style, {
      position: 'absolute', left: '0', top: '0', width: '1px', height: '1px', opacity: '0',
      border: '0', padding: '0', resize: 'none', fontSize: '16px', caretColor: 'transparent',
    });
    ime.value = '  ';
    stage.append(canvas, cursor);
    root.append(stage, ime);
    this.container.append(root);
    this.root = root;
    this.stage = stage;
    this.canvas = canvas;
    this.cursorEl = cursor;
    this.ime = ime;
    // Video goes to a worker with an OffscreenCanvas where possible (opts.worker = false keeps
    // it on this thread). The canvas moves over only once the worker has said it runs and has
    // WebCodecs; a page whose CSP forbids blob: workers falls back to this thread.
    this.worker = null;
    this._sockets = 0;
    this._wsock = null;
    this._videoReady = new Promise((resolve) => {
      const local = () => {
        this._sink = new VideoSink(canvas, {
          send: (d) => (typeof d === 'string' ? this.ws && this.ws.readyState === 1 && this.ws.send(d) : this._sendBin(d)),
          event: (ev) => this._onSinkEvent(ev),
        });
        resolve();
      };
      if (this.opts.worker === false || typeof Worker === 'undefined' || !('transferControlToOffscreen' in canvas)) {
        local();
        return;
      }
      let worker;
      const url = URL.createObjectURL(new Blob([videoWorkerSource()], { type: 'text/javascript' }));
      try {
        worker = new Worker(url);
      } catch {
        URL.revokeObjectURL(url);
        local();
        return;
      }
      let settled = false;
      const fallback = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        URL.revokeObjectURL(url);
        worker.terminate();
        local();
      };
      const timer = setTimeout(fallback, 3000);
      worker.onerror = fallback;
      worker.onmessage = (e) => {
        if (settled) return;
        if (e.data.t !== 'ready') {
          fallback();
          return;
        }
        settled = true;
        clearTimeout(timer);
        URL.revokeObjectURL(url);
        const offscreen = canvas.transferControlToOffscreen();
        worker.postMessage({ t: 'canvas', canvas: offscreen }, [offscreen]);
        worker.onerror = null;
        worker.onmessage = (ev) => this._onWorker(ev.data);
        this.worker = worker;
        resolve();
      };
    });
    this._ro = new ResizeObserver(() => this._layout());
    this._ro.observe(root);
  }

  _layout() {
    const r = this.root.getBoundingClientRect();
    const w = this.width || 16;
    const h = this.height || 9;
    const dpr = window.devicePixelRatio || 1;
    let cssW;
    let cssH;
    if (this.opts.scale === 'native') {
      cssW = w / dpr;
      cssH = h / dpr;
    } else {
      const k = Math.min(r.width / w, r.height / h);
      cssW = w * k;
      cssH = h * k;
    }
    this.fit = { left: Math.max(0, (r.width - cssW) / 2), top: Math.max(0, (r.height - cssH) / 2), w: cssW, h: cssH };
    this.canvas.style.width = `${cssW}px`;
    this.canvas.style.height = `${cssH}px`;
    // One stream pixel on a whole number of device pixels stays sharp with nearest-neighbour.
    const ratio = (cssW * dpr) / w;
    const whole = Math.abs(ratio - Math.round(ratio)) < 0.01 && ratio >= 1;
    this.canvas.style.imageRendering = whole ? 'pixelated' : 'auto';
    this._applyView();
  }

  _applyView() {
    const v = this.view;
    const f = this.fit || { left: 0, top: 0 };
    this.stage.style.transform = `translate(${f.left + v.tx}px, ${f.top + v.ty}px) scale(${v.scale})`;
    this._placeCursor();
  }

  // ---------------------------------------------------------------- connection

  _setState(state, detail) {
    this.state = state;
    this.dispatchEvent(new CustomEvent('state', { detail: { state, detail } }));
  }

  _open(url, ticket) {
    return new Promise((resolve, reject) => {
      const ws = this.worker ? (this._wsock = new WorkerSocket(this.worker, url, ++this._sockets)) : new WebSocket(url);
      ws.binaryType = 'arraybuffer';
      let settled = false;
      let opened = false;
      const fail = (msg, extra) => Object.assign(new Error(msg), { beforeOpen: !opened, ticketSpent: opened }, extra);
      const timer = setTimeout(() => {
        if (!settled) {
          settled = true;
          ws.close();
          reject(fail(`timed out connecting to ${url}`));
        }
      }, 6000);
      ws.onopen = () => {
        opened = true;
        this._url = url;
        ws.send(JSON.stringify({
          t: 'hello',
          ticket,
          codecs: this.opts.codecs,
          maxFps: this.opts.maxFps || this.refreshHz || 60,
          maxBitrate: this.opts.maxBitrate,
          monitor: this.opts.monitor,
        }));
      };
      ws.onmessage = (ev) => {
        this.received.messages++;
        this.received.bytes += typeof ev.data === 'string' ? ev.data.length : ev.data.byteLength;
        if (typeof ev.data === 'string') {
          const m = JSON.parse(ev.data);
          if (!settled) {
            if (m.t === 'welcome') {
              settled = true;
              clearTimeout(timer);
              this.ws = ws;
              this._onWelcome(m);
              resolve();
            } else if (m.t === 'error') {
              settled = true;
              clearTimeout(timer);
              reject(fail(m.message, { beforeOpen: false }));
            }
            return;
          }
          this._onJson(m);
        } else {
          this._onBinary(ev.data);
        }
      };
      ws.onclose = () => {
        clearTimeout(timer);
        if (!settled) {
          settled = true;
          reject(fail(`could not connect to ${url}`));
          return;
        }
        if (this.ws === ws) {
          this.ws = null;
          this._teardown();
          if (!this._closing) {
            this._setState('disconnected', this._lastError || 'connection lost');
            this._scheduleReconnect();
          }
        }
      };
    });
  }

  /**
   * The first try after a working stream drops goes at once: through a tunnel that is usually a
   * blip, and the next connection works. Later tries back off. `retry` only starts over once a new
   * connection shows a frame, so a session that fails right after its welcome backs off too.
   */
  _scheduleReconnect() {
    if (!this.opts.reconnect || this._closing || typeof this.opts.ticket !== 'function') return;
    const delay = [0, 500, 1000, 2000, 5000, 10000][Math.min(this.retry++, 5)];
    clearTimeout(this._reconnectTimer);
    this._reconnectTimer = setTimeout(() => this.connect(), delay);
  }

  _teardown() {
    clearInterval(this._pingTimer);
    clearInterval(this._statsTimer);
    this._releaseAll();
    if (this.inputWs) this.inputWs.close();
    if (this._inputReady) this._inputReady.close();
    this.inputWs = null;
    this._inputReady = null;
    clearTimeout(this._inputTimer);
    clearTimeout(this._inputRetry);
    if (this._sink) this._sink.close();
    this.decoderConfig = null;
  }

  _send(obj) {
    if (this.ws && this.ws.readyState === 1) this.ws.send(JSON.stringify(obj));
  }

  _sendBin(buf) {
    if (this.ws && this.ws.readyState === 1) this.ws.send(buf);
  }

  _onWelcome(m) {
    this.welcome = m;
    this.monitors = m.monitors;
    this.monitor = m.monitor;
    this._lastError = null;
    this._setState('connected', m);
    this.dispatchEvent(new CustomEvent('monitors', { detail: { monitors: m.monitors, current: m.monitor } }));
    this._lastPointer = null;
    this._held = new Set();
    this._lastDiscrete = 0;
    this._inputTries = 0;
    if (m.inputToken) this._openInput(m.inputToken);
    this._ping();
    this._pingTimer = setInterval(() => this._ping(), 1000);
    this._connectedAt = performance.now();
    this._statsTimer = setInterval(() => {
      this._publishStats();
      this._watchdog();
    }, 1000);
    this._updateCursorMode();
  }

  /**
   * Input gets its own socket so a click never queues behind acks or control messages. Input
   * starts on the main socket and moves to its own socket once that is open, at a moment when
   * nothing is pressed and no button or key has changed for a while. Two sockets do not keep order
   * between them, so a press must never go on one and its release on the other (a lost click or a
   * stuck button right after connecting). Through a tunnel the second socket can take a second to
   * open, and tunnels drop one now and then; input works on the main socket meanwhile, and a socket
   * that closes is replaced: the client asks for a fresh one-time token (`input-token`) and opens
   * another, backing off from 200 ms to 5 s while they keep failing.
   */
  _openInput(token) {
    const main = this.ws;
    const url = this._url.replace(/\/ws(\?.*)?$/, '/input');
    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    let opened = 0;
    ws.onopen = () => {
      opened = performance.now();
      ws.send(JSON.stringify({ token }));
      this._inputReady = ws;
      this._maybeSwitchInput();
    };
    ws.onclose = () => {
      if (this.inputWs === ws) this.inputWs = null;
      if (this._inputReady === ws) this._inputReady = null;
      if (!main || this.ws !== main) return; // the session itself ended
      // One that worked for a while starts the backoff over.
      if (opened && performance.now() - opened > 10000) this._inputTries = 0;
      const delay = Math.min(5000, 200 * 2 ** this._inputTries++);
      clearTimeout(this._inputRetry);
      this._inputRetry = setTimeout(() => this.ws === main && this._send({ t: 'input-token' }), delay);
    };
  }

  /** Move input to its own socket when doing so cannot reorder a press and its release. */
  _maybeSwitchInput() {
    const ws = this._inputReady;
    if (!ws || ws.readyState !== 1) return;
    const quiet = performance.now() - this._lastDiscrete;
    if (this._held.size === 0 && quiet >= 300) {
      this.inputWs = ws;
      this._inputReady = null;
    } else {
      clearTimeout(this._inputTimer);
      this._inputTimer = setTimeout(() => this._maybeSwitchInput(), Math.max(50, 300 - quiet));
    }
  }

  _sendInput(buf) {
    const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
    if (b[0] === IN_BUTTON || b[0] === IN_KEY || b[0] === IN_TEXT || b[0] === IN_TOUCH) {
      this._lastDiscrete = performance.now();
      // What is pressed: buttons, keys, and touches between down (0) and up or cancel (2, 3).
      const held =
        b[0] === IN_BUTTON ? [`b${b[1]}`, b[2] === 1]
        : b[0] === IN_KEY ? [`k${b[2] | (b[3] << 8)}`, b[1] === 1]
        : b[0] === IN_TOUCH && b[1] !== 1 ? [`t${b[2]}`, b[1] === 0]
        : null;
      if (held && this._held) {
        if (held[1]) this._held.add(held[0]);
        else this._held.delete(held[0]);
      }
    }
    const ws = this.inputWs && this.inputWs.readyState === 1 ? this.inputWs : this.ws;
    if (ws && ws.readyState === 1) ws.send(buf);
    if (this._inputReady) this._maybeSwitchInput();
  }

  _ping() {
    this._send({ t: 'ping', id: (this._pingId = (this._pingId || 0) + 1), c: performance.now() });
  }

  _onJson(m) {
    switch (m.t) {
      case 'pong': {
        const now = performance.now();
        const rtt = now - m.c;
        // Server clock (µs) minus client clock (ms), from the tightest recent round trip.
        const offset = m.s / 1000 - (m.c + rtt / 2);
        const c = this.clock;
        c.samples.push({ rtt, offset });
        if (c.samples.length > 30) c.samples.shift();
        const best = c.samples.reduce((a, b) => (b.rtt < a.rtt ? b : a));
        c.offset = best.offset;
        c.bestRtt = best.rtt;
        c.rtt = rtt;
        break;
      }
      case 'stream':
        this.streamInfo = m;
        this.monitor = m.monitor;
        this.decoderConfig = null;
        if (this._sink) this._sink.reset();
        this.dispatchEvent(new CustomEvent('monitors', { detail: { monitors: this.monitors, current: m.monitor } }));
        break;
      case 'stats':
        this.serverStats = m;
        break;
      case 'clipboard':
        this._onRemoteClipboard(m.text);
        break;
      case 'bench':
        if (this._benchReply) this._benchReply(m);
        break;
      case 'input-token':
        if (!this.inputWs && !this._inputReady) this._openInput(m.token);
        break;
      case 'error':
        this._lastError = m.message;
        this.dispatchEvent(new CustomEvent('error', { detail: m.message }));
        break;
      default:
    }
  }

  // ---------------------------------------------------------------- video

  _onBinary(buf) {
    const b = new Uint8Array(buf);
    const dv = new DataView(buf);
    if (b[0] === MSG_VIDEO) {
      this._sink.onVideo(buf);
    } else if (b[0] === MSG_CURSOR_SHAPE) {
      const w = dv.getUint16(8, true);
      const h = dv.getUint16(10, true);
      this._setCursorShape(w, h, dv.getUint16(12, true), dv.getUint16(14, true), b.subarray(16, 16 + w * h * 4));
    } else if (b[0] === MSG_CURSOR_POS) {
      this.cursor.visible = b[1] === 1;
      this.cursor.x = dv.getInt32(4, true);
      this.cursor.y = dv.getInt32(8, true);
      this._updateCursorMode();
    }
  }

  /** Messages from the video worker: socket events for `_open`, the rest as sink events. */
  _onWorker(m) {
    const s = this._wsock;
    const mine = s && m.id === s.id;
    if (m.t === 'open') {
      if (mine) {
        s.readyState = 1;
        if (s.onopen) s.onopen();
      }
    } else if (m.t === 'text' || m.t === 'binary') {
      if (mine && s.onmessage) s.onmessage({ data: m.data });
    } else if (m.t === 'close') {
      if (mine) {
        s.readyState = 3;
        this._wsock = null;
        if (s.onclose) s.onclose();
      }
    } else if (mine) {
      if (m.t === 'frame') {
        this.received.messages++;
        this.received.bytes += m.bytes;
      }
      this._onSinkEvent(m);
    } else if (m.t === 'vframe') {
      m.frame.close();
    }
  }

  _onSinkEvent(m) {
    const local = (t) => t - performance.timeOrigin;
    switch (m.t) {
      case 'size':
        this.width = m.width;
        this.height = m.height;
        this.vcursor = { x: m.width / 2, y: m.height / 2 };
        this._layout();
        break;
      case 'config':
        this.decoderConfig = { codec: m.codec };
        break;
      case 'error':
        this.dispatchEvent(new CustomEvent('error', { detail: m.message }));
        break;
      case 'health':
        this._onHealth(m);
        break;
      case 'vframe': {
        const now = local(m.decoded);
        const meta = m.meta && { ...m.meta, recv: local(m.meta.recv) };
        if (this.bench || this._benchWatch) this._benchCheck(m.frame, now, meta);
        if (this._timecode) this._timecodeCheck(m.frame, now);
        m.frame.close();
        break;
      }
      case 'frame': {
        this.retry = 0;
        const now = local(m.decoded);
        const recv = local(m.recv);
        const w = this.win;
        w.frames++;
        w.bytes += m.bytes;
        if (w.lastFrame) w.intervals.push(now - w.lastFrame);
        w.lastFrame = now;
        w.decode.push(now - recv);
        if (!m.refine) {
          const captureLocal = m.captureUs / 1000 - this.clock.offset;
          const encoded = captureLocal + (m.queueUs + m.encodeUs) / 1000;
          w.network.push(Math.max(0, recv - encoded));
          w.server.push((m.queueUs + m.encodeUs) / 1000);
          requestAnimationFrame((t) => {
            // rAF time is when this frame's composite begins; the drawn canvas shows with it.
            w.e2e.push(Math.max(t, now) - captureLocal);
            w.display.push(Math.max(0, t - now));
          });
        } else {
          w.refines++;
        }
        break;
      }
      default:
    }
  }

  /** Hand decoded frames to the latency measurements while one runs. */
  _watchFrames(on) {
    if (this.worker) this.worker.postMessage({ t: 'watch', on });
    else this._sink.watch = on;
  }

  // ---------------------------------------------------------------- watchdog

  /** Once a second: let the video sink check that frames reach the screen (see VideoSink.check). */
  _watchdog() {
    if (this.state !== 'connected') return;
    const luma = this.serverStats.lumaMax ?? 0;
    const connectedMs = performance.now() - this._connectedAt;
    if (this.worker) this.worker.postMessage({ t: 'check', luma, connectedMs });
    else if (this._sink) this._sink.check(luma, connectedMs);
  }

  /** `health` events: an issue the first time it shows, `issue: null` once it clears. */
  _onHealth(m) {
    if (m.issue === this._healthIssue) return;
    this._healthIssue = m.issue;
    if (m.issue) console.warn(`skylight: ${m.detail}`);
    this.dispatchEvent(new CustomEvent('health', { detail: { issue: m.issue, detail: m.detail, final: m.final } }));
  }

  _newWindow() {
    return { start: performance.now(), frames: 0, bytes: 0, refines: 0, decode: [], network: [], server: [], e2e: [], display: [], intervals: [] };
  }

  _publishStats() {
    const w = this.win;
    const secs = (performance.now() - w.start) / 1000;
    const s = this.serverStats;
    const jitter = w.intervals.length > 1
      ? Math.sqrt(w.intervals.reduce((a, x) => a + (x - median(w.intervals)) ** 2, 0) / w.intervals.length)
      : 0;
    this.stats = {
      fps: w.frames / secs,
      kbps: (w.bytes * 8) / secs / 1000,
      latencyMs: median(w.e2e),
      latencyP95Ms: percentile(w.e2e, 0.95),
      serverMs: median(w.server),
      networkMs: median(w.network),
      decodeMs: median(w.decode),
      displayMs: median(w.display),
      jitterMs: jitter,
      rttMs: this.clock.rtt || 0,
      refines: w.refines,
      targetKbps: s.targetKbps || 0,
      encodeMs: s.encodeMs || 0,
      queueMs: s.queueMs || 0,
      inputToCaptureMs: s.inputToCaptureMs || 0,
      inputToSentMs: s.inputToSentMs || 0,
      skipped: s.skipped || 0,
      encoder: s.encoder || '',
      codec: this.decoderConfig ? this.decoderConfig.codec : '',
      width: this.width,
      height: this.height,
    };
    this.win = this._newWindow();
    this.dispatchEvent(new CustomEvent('stats', { detail: this.stats }));
  }

  _measureRefresh() {
    const times = [];
    const tick = (t) => {
      times.push(t);
      if (times.length < 40) {
        requestAnimationFrame(tick);
        return;
      }
      const d = [];
      for (let i = 1; i < times.length; i++) d.push(times[i] - times[i - 1]);
      const hz = 1000 / median(d);
      this.refreshHz = [60, 75, 90, 100, 120, 144, 165, 240].reduce((a, b) => (Math.abs(b - hz) < Math.abs(a - hz) ? b : a));
      if (this.state === 'connected' && !this.opts.maxFps) this._send({ t: 'settings', maxFps: this.refreshHz });
    };
    requestAnimationFrame(tick);
  }

  // ---------------------------------------------------------------- cursor

  _setCursorShape(w, h, hotX, hotY, rgba) {
    const c = document.createElement('canvas');
    c.width = w;
    c.height = h;
    c.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(rgba), w, h), 0, 0);
    const url = c.toDataURL('image/png');
    Object.assign(this.cursor, { url, hotX, hotY, w, h });
    this._updateCursorMode();
  }

  _touchUI() {
    return this._lastPointerType === 'touch' || this._lastPointerType === 'pen';
  }

  _updateCursorMode() {
    const c = this.cursor;
    const drawn = this._touchUI() && this.opts.touchMode === 'trackpad';
    if (drawn || !c.url) {
      this.canvas.style.cursor = this.opts.viewOnly ? 'default' : c.url || !c.visible ? 'none' : 'default';
    } else {
      this.canvas.style.cursor = c.visible ? `url(${c.url}) ${c.hotX} ${c.hotY}, default` : 'none';
    }
    if (this.opts.viewOnly && c.url) this.canvas.style.cursor = 'default';
    const showSprite = c.url && c.visible && (drawn || this.opts.viewOnly || (this._touchUI() && this.opts.touchMode === 'direct'));
    this.cursorEl.style.display = showSprite ? 'block' : 'none';
    if (showSprite) {
      this.cursorEl.style.backgroundImage = `url(${c.url})`;
      this._placeCursor();
    }
  }

  _placeCursor() {
    const c = this.cursor;
    if (!c.url || !this.width || !this.fit) return;
    const k = this.fit.w / this.width;
    const pos = this._touchUI() && this.opts.touchMode === 'trackpad' ? this.vcursor : c;
    const el = this.cursorEl.style;
    // Draw the cursor at its true size on screen, whatever the zoom.
    const size = 1 / this.view.scale;
    el.width = `${c.w * size}px`;
    el.height = `${c.h * size}px`;
    el.transform = `translate(${pos.x * k - c.hotX * size}px, ${pos.y * k - c.hotY * size}px)`;
  }

  // ---------------------------------------------------------------- input

  _canInput() {
    return !this.opts.viewOnly && this.welcome && this.welcome.input;
  }

  _toStream(clientX, clientY) {
    const r = this.canvas.getBoundingClientRect();
    return {
      x: ((clientX - r.left) / r.width) * this.width,
      y: ((clientY - r.top) / r.height) * this.height,
    };
  }

  _pointer(x, y) {
    // Only moves that land somewhere new (pointer events fire for sub-pixel moves, and with
    // pointerrawupdate at the mouse's own rate).
    const qx = Math.round(x * 2) / 2;
    const qy = Math.round(y * 2) / 2;
    if (this._lastPointer && this._lastPointer.x === qx && this._lastPointer.y === qy) return;
    this._lastPointer = { x: qx, y: qy };
    const m = new ArrayBuffer(12);
    const dv = new DataView(m);
    dv.setUint8(0, IN_POINTER_ABS);
    dv.setFloat32(4, x, true);
    dv.setFloat32(8, y, true);
    this._sendInput(m);
  }

  _button(button, down) {
    this._sendInput(new Uint8Array([IN_BUTTON, button, down ? 1 : 0, 0]));
  }

  _scroll(dx, dy) {
    const m = new ArrayBuffer(12);
    const dv = new DataView(m);
    dv.setUint8(0, IN_SCROLL);
    dv.setFloat32(4, dx, true);
    dv.setFloat32(8, dy, true);
    this._sendInput(m);
  }

  _key(code, down) {
    if (down) this.pressedKeys.add(code);
    else this.pressedKeys.delete(code);
    this._sendInput(new Uint8Array([IN_KEY, down ? 1 : 0, code & 0xff, code >> 8]));
  }

  _touch(kind, id, x, y) {
    const m = new ArrayBuffer(12);
    const dv = new DataView(m);
    dv.setUint8(0, IN_TOUCH);
    dv.setUint8(1, kind);
    dv.setUint8(2, id);
    dv.setFloat32(4, x, true);
    dv.setFloat32(8, y, true);
    this._sendInput(m);
  }

  _releaseAll() {
    for (const k of [...this.pressedKeys]) this._key(k, false);
    if (this._buttonsDown) {
      for (const b of this._buttonsDown) this._button(b, false);
      this._buttonsDown.clear();
    }
  }

  _mapKey(code) {
    let k = KEYMAP[code];
    if (k === undefined) return undefined;
    if (isApple && this.opts.cmdKey === 'ctrl') {
      if (k === 125) k = KEY_LEFTCTRL;
      else if (k === 126) k = KEY_RIGHTCTRL;
    }
    return k;
  }

  _bindInput() {
    const c = this.canvas;
    const root = this.root;
    this._buttonsDown = new Set();
    const moveEvent = 'onpointerrawupdate' in window ? 'pointerrawupdate' : 'pointermove';

    root.addEventListener('contextmenu', (e) => e.preventDefault());
    root.addEventListener('pointerdown', (e) => {
      this._lastPointerType = e.pointerType;
      this._updateCursorMode();
      if (e.pointerType === 'touch') return;
      this.focus();
      if (!this._canInput()) return;
      e.preventDefault();
      try {
        root.setPointerCapture(e.pointerId);
      } catch {
        // Not an active pointer (a synthetic event); input still goes through.
      }
      const p = this._toStream(e.clientX, e.clientY);
      this._pointer(p.x, p.y);
      if (e.button >= 0 && e.button <= 4) {
        this._buttonsDown.add(e.button);
        this._button(e.button, true);
      }
    });
    root.addEventListener('pointerup', (e) => {
      if (e.pointerType === 'touch' || !this._canInput()) return;
      if (this._buttonsDown.delete(e.button)) this._button(e.button, false);
    });
    root.addEventListener(moveEvent, (e) => {
      if (e.pointerType === 'touch' || !this._canInput()) return;
      const p = this._toStream(e.clientX, e.clientY);
      this._pointer(p.x, p.y);
    });
    root.addEventListener('wheel', (e) => {
      e.preventDefault();
      if (!this._canInput()) return;
      // One wheel notch is about 100 px in browsers; lines come in threes.
      let k = 1 / 100;
      if (e.deltaMode === 1) k = 1 / 3;
      else if (e.deltaMode === 2) k = 10;
      const p = this._toStream(e.clientX, e.clientY);
      this._pointer(p.x, p.y);
      if (e.ctrlKey && !this.pressedKeys.has(KEY_LEFTCTRL)) {
        // A trackpad pinch: forward as Ctrl+scroll, which apps treat as zoom.
        this._key(KEY_LEFTCTRL, true);
        this._scroll(e.deltaX * k, e.deltaY * k);
        this._key(KEY_LEFTCTRL, false);
      } else {
        this._scroll(e.deltaX * k, e.deltaY * k);
      }
    }, { passive: false });

    // Keyboard, through the hidden text field.
    const ime = this.ime;
    ime.addEventListener('keydown', (e) => {
      if (!this._canInput()) return;
      if (!e.code || e.code === 'Unidentified' || e.isComposing) return; // phone keyboards: see beforeinput
      const k = this._mapKey(e.code);
      if (k === undefined) return;
      const shortcut = isApple ? e.metaKey && this.opts.cmdKey === 'ctrl' : e.ctrlKey;
      if (shortcut && e.code === 'KeyV' && this.opts.clipboard && this.welcome.clipboard) {
        // Let the browser fire a paste event: it carries the local clipboard without a prompt.
        return;
      }
      e.preventDefault();
      // The remote desktop repeats held keys itself.
      if (e.repeat) return;
      if (shortcut && (e.code === 'KeyC' || e.code === 'KeyX')) this._armCopy();
      this._key(k, true);
      // macOS sends no keyup for keys released while Cmd is held: release them right away.
      if (isApple && e.metaKey && !MODIFIERS.has(k)) this._key(k, false);
      if (e.code === 'CapsLock') this._key(k, false);
    });
    ime.addEventListener('keyup', (e) => {
      if (!this._canInput() || !e.code) return;
      const k = this._mapKey(e.code);
      if (k === undefined) return;
      e.preventDefault();
      if (e.code === 'CapsLock') {
        this._key(k, true);
        this._key(k, false);
        return;
      }
      if (this.pressedKeys.has(k)) this._key(k, false);
    });
    ime.addEventListener('paste', (e) => {
      e.preventDefault();
      if (!this._canInput()) return;
      const text = e.clipboardData ? e.clipboardData.getData('text/plain') : '';
      if (text && this.welcome.clipboard) this._send({ t: 'clipboard', text });
      // Then press V under the Ctrl that is already held (or a whole Ctrl+V from a menu paste),
      // once the remote clipboard holds the text.
      setTimeout(() => {
        if (this.pressedKeys.has(KEY_LEFTCTRL) || this.pressedKeys.has(KEY_RIGHTCTRL)) this.sendKeys([KEYMAP.KeyV]);
        else this.sendKeys([KEY_LEFTCTRL, KEYMAP.KeyV]);
      }, 30);
    });
    ime.addEventListener('beforeinput', (e) => {
      e.preventDefault();
      if (!this._canInput()) return;
      if (e.inputType === 'insertText' || e.inputType === 'insertReplacementText') {
        this.typeText(e.data || '');
      } else if (e.inputType === 'deleteContentBackward') {
        this.sendKeys([KEY_BACKSPACE]);
      } else if (e.inputType === 'insertLineBreak' || e.inputType === 'insertParagraph') {
        this.sendKeys([KEY_ENTER]);
      }
    });
    ime.addEventListener('input', () => {
      ime.value = '  ';
    });
    ime.addEventListener('blur', () => this._releaseAll());
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) this._releaseAll();
      if (this.state === 'connected') {
        this._send({ t: 'settings', maxFps: document.hidden ? 5 : this.opts.maxFps || this.refreshHz || 60 });
      }
    });

    this._bindTouch();
  }

  _armCopy() {
    // Safari only lets a page write the clipboard during a user gesture. Hand it a promise now
    // and fill it when the remote desktop reports its new clipboard.
    if (!navigator.clipboard || !window.ClipboardItem || !this.welcome || !this.welcome.clipboard) return;
    const text = new Promise((resolve) => {
      const timer = setTimeout(() => {
        this._copyWaiter = null;
        resolve(this._remoteClipboard || '');
      }, 1500);
      this._copyWaiter = (t) => {
        clearTimeout(timer);
        resolve(t);
      };
    });
    const blob = text.then((t) => new Blob([t], { type: 'text/plain' }));
    navigator.clipboard.write([new ClipboardItem({ 'text/plain': blob })]).catch(() => {});
  }

  _onRemoteClipboard(text) {
    this._remoteClipboard = text;
    this.dispatchEvent(new CustomEvent('clipboard', { detail: text }));
    if (this._copyWaiter) {
      const w = this._copyWaiter;
      this._copyWaiter = null;
      w(text);
    } else if (this.opts.clipboard && navigator.clipboard && document.hasFocus()) {
      navigator.clipboard.writeText(text).catch(() => {});
    }
  }

  // ---------------------------------------------------------------- touch

  _bindTouch() {
    const root = this.root;
    const touches = new Map();
    let gesture = null;
    let lastTap = 0;

    const pinchState = () => {
      const [a, b] = [...touches.values()];
      return { dist: Math.hypot(a.x - b.x, a.y - b.y), mx: (a.x + b.x) / 2, my: (a.y + b.y) / 2 };
    };

    root.addEventListener('pointerdown', (e) => {
      if (e.pointerType !== 'touch') return;
      e.preventDefault();
      try {
        root.setPointerCapture(e.pointerId);
      } catch {
        // See above.
      }
      touches.set(e.pointerId, { x: e.clientX, y: e.clientY, sx: e.clientX, sy: e.clientY, t: performance.now() });
      if (touches.size === 1) {
        const now = performance.now();
        gesture = { kind: 'one', start: now, moved: false, dragging: now - lastTap < 300, longTimer: 0 };
        if (this.opts.touchMode === 'direct') {
          gesture.longTimer = setTimeout(() => {
            if (gesture && !gesture.moved && this._canInput()) {
              gesture.long = true;
              const p = this._toStream(e.clientX, e.clientY);
              this._pointer(p.x, p.y);
              this._button(0, true);
              gesture.dragging = true;
            }
          }, 450);
        } else if (gesture.dragging && this._canInput()) {
          this._pointer(this.vcursor.x, this.vcursor.y);
          this._button(0, true);
        }
      } else if (touches.size === 2) {
        if (gesture && gesture.longTimer) clearTimeout(gesture.longTimer);
        if (gesture && gesture.dragging && this._canInput()) this._button(0, false);
        const ps = pinchState();
        gesture = { kind: 'two', start: performance.now(), moved: false, ...ps, scale0: this.view.scale, zoomed: false };
      }
    });

    root.addEventListener('pointermove', (e) => {
      if (e.pointerType !== 'touch') return;
      const t = touches.get(e.pointerId);
      if (!t || !gesture) return;
      const dx = e.clientX - t.x;
      const dy = e.clientY - t.y;
      t.x = e.clientX;
      t.y = e.clientY;
      if (Math.hypot(t.x - t.sx, t.y - t.sy) > 8) gesture.moved = true;
      if (gesture.kind === 'one') {
        if (this.opts.touchMode === 'trackpad') {
          if (!this._canInput()) return;
          // Relative pointer, a little faster than the finger, like a laptop trackpad.
          const k = (this.width / this.fit.w / this.view.scale) * 1.6;
          this.vcursor.x = Math.min(this.width - 1, Math.max(0, this.vcursor.x + dx * k));
          this.vcursor.y = Math.min(this.height - 1, Math.max(0, this.vcursor.y + dy * k));
          this._pointer(this.vcursor.x, this.vcursor.y);
          this._placeCursor();
          this._keepCursorInView();
        } else if (gesture.dragging) {
          const p = this._toStream(e.clientX, e.clientY);
          this._pointer(p.x, p.y);
        } else if (gesture.moved) {
          if (gesture.longTimer) clearTimeout(gesture.longTimer);
          if (this.view.scale > 1.01) this._pan(dx, dy);
          else if (this._canInput()) this._scroll(0, -dy / 40);
        }
      } else if (gesture.kind === 'two' && touches.size === 2) {
        const ps = pinchState();
        const ratio = ps.dist / gesture.dist;
        if (Math.abs(ratio - 1) > 0.08 || gesture.zoomed) {
          gesture.zoomed = true;
          this._zoomAt(gesture.scale0 * ratio, ps.mx, ps.my);
          this._pan(ps.mx - gesture.mx, ps.my - gesture.my);
        } else if (this._canInput()) {
          const sdx = ps.mx - gesture.mx;
          const sdy = ps.my - gesture.my;
          if (Math.abs(sdx) + Math.abs(sdy) > 0) this._scroll(-sdx / 40, -sdy / 40);
        }
        gesture.mx = ps.mx;
        gesture.my = ps.my;
        if (gesture.zoomed) gesture.dist = ps.dist, gesture.scale0 = this.view.scale;
      }
    });

    const end = (e) => {
      if (e.pointerType !== 'touch') return;
      const t = touches.get(e.pointerId);
      touches.delete(e.pointerId);
      if (!gesture || !t) return;
      if (gesture.longTimer) clearTimeout(gesture.longTimer);
      const quick = performance.now() - gesture.start < 250;
      if (gesture.kind === 'one' && touches.size === 0) {
        if (gesture.dragging && this._canInput()) {
          this._button(0, false);
        } else if (quick && !gesture.moved && this._canInput()) {
          if (this.opts.touchMode === 'direct') {
            const p = this._toStream(t.x, t.y);
            this._pointer(p.x, p.y);
          } else {
            this._pointer(this.vcursor.x, this.vcursor.y);
          }
          this._button(0, true);
          this._button(0, false);
          lastTap = performance.now();
        }
        gesture = null;
      } else if (gesture.kind === 'two' && touches.size === 0) {
        if (quick && !gesture.moved && !gesture.zoomed && this._canInput()) {
          if (this.opts.touchMode === 'direct') {
            const p = this._toStream(t.x, t.y);
            this._pointer(p.x, p.y);
          } else {
            this._pointer(this.vcursor.x, this.vcursor.y);
          }
          this._button(2, true);
          this._button(2, false);
        }
        gesture = null;
      }
    };
    root.addEventListener('pointerup', end);
    root.addEventListener('pointercancel', end);
  }

  _zoomAt(scale, cx, cy) {
    const r = this.root.getBoundingClientRect();
    const v = this.view;
    const s = Math.min(6, Math.max(1, scale));
    const f = this.fit;
    // Keep the point under the fingers fixed.
    const px = cx - r.left - f.left - v.tx;
    const py = cy - r.top - f.top - v.ty;
    v.tx -= px * (s / v.scale - 1);
    v.ty -= py * (s / v.scale - 1);
    v.scale = s;
    this._clampView();
  }

  _pan(dx, dy) {
    this.view.tx += dx;
    this.view.ty += dy;
    this._clampView();
  }

  _clampView() {
    const v = this.view;
    const f = this.fit;
    const r = this.root.getBoundingClientRect();
    if (v.scale <= 1.001) {
      v.scale = 1;
      v.tx = 0;
      v.ty = 0;
    } else {
      const w = f.w * v.scale;
      const h = f.h * v.scale;
      v.tx = Math.min(-f.left, Math.max(r.width - f.left - w, v.tx));
      v.ty = Math.min(-f.top, Math.max(r.height - f.top - h, v.ty));
      if (w < r.width) v.tx = (r.width - w) / 2 - f.left;
      if (h < r.height) v.ty = (r.height - h) / 2 - f.top;
    }
    this._applyView();
  }

  _keepCursorInView() {
    if (this.view.scale <= 1) return;
    const r = this.root.getBoundingClientRect();
    const f = this.fit;
    const k = (f.w / this.width) * this.view.scale;
    const x = f.left + this.view.tx + this.vcursor.x * k;
    const y = f.top + this.view.ty + this.vcursor.y * k;
    const m = 40;
    let dx = 0;
    let dy = 0;
    if (x < m) dx = m - x;
    if (x > r.width - m) dx = r.width - m - x;
    if (y < m) dy = m - y;
    if (y > r.height - m) dy = r.height - m - y;
    if (dx || dy) this._pan(dx, dy);
  }

  // ---------------------------------------------------------------- latency probe

  _benchOnce(act, probe, threshold, wantHigh, timeoutMs) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.bench = null;
        reject(new Error('no response from the probe'));
      }, timeoutMs);
      const t0 = performance.now();
      this.bench = {
        t0,
        probe,
        test: (level) => (wantHigh ? level > threshold : level < threshold),
        done: (decodedAt) => {
          clearTimeout(timer);
          this.bench = null;
          // The frame was drawn at decodedAt; it reaches the screen with the first animation
          // frame after that. The readback finished later, so look that frame up.
          const settle = () => {
            const t = this._rafTimes.find((x) => x >= decodedAt);
            if (t !== undefined) resolve({ decoded: decodedAt - t0, shown: t - t0 });
            else requestAnimationFrame(settle);
          };
          settle();
        },
      };
      act();
    });
  }

  _benchCheck(frame, now, meta) {
    const b = this.bench || this._benchWatch;
    if (!b) return;
    // Read the probe's pixels straight from the decoded frame. copyTo is asynchronous and
    // never stalls rendering (a canvas getImageData would force a GPU readback every frame and
    // add its own delay to the very thing being measured). The decode time recorded is `now`.
    const [x, y, w, h] = b.probe;
    const even = (v) => Math.max(0, Math.floor(v / 2) * 2);
    const rect = { x: even(x + w * 0.3), y: even(y + h * 0.3), width: even(w * 0.4), height: even(h * 0.4) };
    const copy = frame.clone();
    const size = copy.allocationSize({ rect });
    const buf = new Uint8Array(size);
    const job = (this._benchChain || Promise.resolve()).then(async () => {
      try {
        const layout = await copy.copyTo(buf, { rect });
        const fmt = copy.format || '';
        let sum = 0;
        let n = 0;
        if (fmt.startsWith('I4') || fmt.startsWith('NV12')) {
          const { offset, stride } = layout[0];
          for (let r = 0; r < rect.height; r += 2) {
            for (let c = 0; c < rect.width; c += 2) {
              sum += buf[offset + r * stride + c];
              n++;
            }
          }
        } else {
          const { offset, stride } = layout[0];
          for (let r = 0; r < rect.height; r += 2) {
            for (let c = 0; c < rect.width; c += 2) {
              const i = offset + r * stride + c * 4;
              sum += (buf[i] + buf[i + 1] + buf[i + 2]) / 3;
              n++;
            }
          }
        }
        const level = sum / Math.max(1, n);
        this._probeLevel = level;
        const active = this.bench;
        if (active && active.test && active.test(level) && now >= active.t0) {
          if (meta && this._benchLog) {
            this._benchLog.push({
              t0: active.t0,
              captured: meta.captureUs / 1000 - this.clock.offset,
              received: meta.recv,
              decoded: now,
              serverMs: (meta.queueUs + meta.encodeUs) / 1000,
              bytes: meta.bytes,
            });
          }
          active.done(now);
        }
      } finally {
        copy.close();
      }
    });
    this._benchChain = job.catch(() => {});
  }
}
