// A TCP proxy that behaves like a slower network path, for benchmarking without one:
// a fixed one-way delay each way, and optionally a bottleneck rate with a deep queue (the
// bufferbloat a home router or a relay adds), so a sender that ignores the link's capacity sees
// its latency grow exactly as it would on the real thing.
//
//   node scripts/netem.mjs --listen 5591 --to 127.0.0.1:5491 --rtt 47 [--mbps 10] [--queue-ms 2000]
//                          [--jitter 30] [--spike 0.02:120] [--swing 35:4000]
//                          [--kill /input:500[:1]] [--hang /ws:3000[:1]] [--reset 0.1]
//
// --jitter adds up to that many ms to each chunk's one-way delay, at random, in order (like a relay
// that sometimes holds data back), without reordering anything. --spike p:ms holds a share p of
// chunks back by up to that much more (a relay's occasional stall). --swing ms:period moves the
// round trip itself up by as much as ms and back, over each period (ms), the way a relay's
// latency drifts for seconds at a time.
//
// Tunnels also lose connections. --kill path:ms[:n] closes the first n connections (default 1)
// whose HTTP request is for path, ms after they open (a socket that dies once it is in use);
// --hang path:ms[:n] instead stops forwarding them, both ways, and leaves both ends open (a path
// that went dead without a word, which only a timeout notices); --reset p resets a share p of new
// connections before anything gets through.
import net from 'node:net';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : def;
};
const listen = Number(arg('listen', 5591));
const [host, port] = arg('to', '127.0.0.1:5491').split(':');
const oneWay = Number(arg('rtt', 47)) / 2;
const mbps = Number(arg('mbps', 0));
const queueMs = Number(arg('queue-ms', 2000));
const jitter = Number(arg('jitter', 0));
const [spikeP, spikeMs] = arg('spike', '0:0').split(':').map(Number);
const [swingMs, swingPeriod] = arg('swing', '0:1').split(':').map(Number);
const fault = (name) => {
  const k = arg(name);
  if (!k) return { path: null, after: 0, count: 0, done: 0 };
  const [path, ms, n] = k.split(':');
  return { path, after: Number(ms), count: Number(n || 1), done: 0 };
};
const kill = fault('kill');
const hang = fault('hang');
const resetP = Number(arg('reset', 0));
const t0 = performance.now();

// Extra one-way delay from --swing at time `now`: half the round-trip swing, as a smooth wave.
const swing = (now) => (swingMs / 2) * (1 - Math.cos((2 * Math.PI * (now - t0)) / swingPeriod)) / 2;

// One direction of the path: chunks leave the bottleneck one after another at `mbps`, then
// arrive `oneWay` ms later. Reading from the source pauses while the queue is over `queueMs`.
function pipe(src, dst, conn) {
  const q = [];
  let linkFree = 0;
  let lastAt = 0;
  let timer = null;
  const flush = () => {
    timer = null;
    const now = performance.now();
    if (conn.hung) q.length = 0;
    while (q.length && q[0].at <= now) dst.write(q.shift().data);
    if (q.length) timer = setTimeout(flush, Math.max(0, q[0].at - performance.now()));
    if (src.isPaused() && (q.length === 0 || q.at(-1).at - now < queueMs)) src.resume();
  };
  src.on('data', (data) => {
    if (conn.hung) return;
    const now = performance.now();
    let leaves = now;
    if (mbps > 0) {
      leaves = Math.max(now, linkFree) + (data.length * 8) / (mbps * 1000);
      linkFree = leaves;
    }
    const spike = Math.random() < spikeP ? Math.random() * spikeMs : 0;
    const at = Math.max(lastAt, leaves + oneWay + swing(now) + Math.random() * jitter + spike);
    lastAt = at;
    q.push({ at, data });
    if (!timer) timer = setTimeout(flush, Math.max(0, q[0].at - now));
    if (at - now > queueMs) src.pause();
  });
  src.on('end', () => {
    const wait = q.length ? q.at(-1).at - performance.now() : 0;
    setTimeout(() => dst.end(), Math.max(0, wait) + 1);
  });
  src.on('error', () => dst.destroy());
}

net
  .createServer((client) => {
    if (resetP > 0 && Math.random() < resetP) {
      client.resetAndDestroy();
      return;
    }
    client.setNoDelay(true);
    const server = net.connect(Number(port), host, () => server.setNoDelay(true));
    const conn = { hung: false };
    if (kill.path || hang.path) {
      // The request line arrives in the first chunk: `GET /input HTTP/1.1`.
      client.once('data', (d) => {
        const path = (d.toString('latin1', 0, 256).split(' ')[1] || '').split('?')[0];
        if (path === kill.path && kill.done < kill.count) {
          kill.done++;
          setTimeout(() => {
            client.destroy();
            server.destroy();
          }, kill.after);
        }
        if (path === hang.path && hang.done < hang.count) {
          hang.done++;
          setTimeout(() => (conn.hung = true), hang.after);
        }
      });
    }
    pipe(client, server, conn);
    pipe(server, client, conn);
    server.on('close', () => client.destroy());
    client.on('close', () => server.destroy());
  })
  .listen(listen, '127.0.0.1', () => {
    const extras = [
      mbps && `${mbps} Mbit/s`,
      jitter && `up to ${jitter} ms jitter`,
      swingMs && `swinging up by ${swingMs} ms every ${swingPeriod / 1000} s`,
      kill.path && `closing ${kill.count} ${kill.path} connection(s) after ${kill.after} ms`,
      hang.path && `hanging ${hang.count} ${hang.path} connection(s) after ${hang.after} ms`,
      resetP && `resetting ${resetP * 100}% of new connections`,
    ].filter(Boolean);
    console.log(`127.0.0.1:${listen} -> ${host}:${port}, ${oneWay * 2} ms round trip${extras.map((e) => `, ${e}`).join('')}`);
  });
