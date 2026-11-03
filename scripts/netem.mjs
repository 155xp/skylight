// A TCP proxy that behaves like a slower network path, for benchmarking without one:
// a fixed one-way delay each way, and optionally a bottleneck rate with a deep queue (the
// bufferbloat a home router or a relay adds), so a sender that ignores the link's capacity sees
// its latency grow exactly as it would on the real thing.
//
//   node scripts/netem.mjs --listen 5591 --to 127.0.0.1:5491 --rtt 47 [--mbps 10] [--queue-ms 2000]
//                          [--jitter 30]
//
// --jitter adds up to that many ms to each chunk's one-way delay, at random, in order (like a relay
// that sometimes holds data back), without reordering anything.
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

// One direction of the path: chunks leave the bottleneck one after another at `mbps`, then
// arrive `oneWay` ms later. Reading from the source pauses while the queue is over `queueMs`.
function pipe(src, dst) {
  const q = [];
  let linkFree = 0;
  let lastAt = 0;
  let timer = null;
  const flush = () => {
    timer = null;
    const now = performance.now();
    while (q.length && q[0].at <= now) dst.write(q.shift().data);
    if (q.length) timer = setTimeout(flush, Math.max(0, q[0].at - performance.now()));
    if (src.isPaused() && (q.length === 0 || q.at(-1).at - now < queueMs)) src.resume();
  };
  src.on('data', (data) => {
    const now = performance.now();
    let leaves = now;
    if (mbps > 0) {
      leaves = Math.max(now, linkFree) + (data.length * 8) / (mbps * 1000);
      linkFree = leaves;
    }
    const at = Math.max(lastAt, leaves + oneWay + Math.random() * jitter);
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
    client.setNoDelay(true);
    const server = net.connect(Number(port), host, () => server.setNoDelay(true));
    pipe(client, server);
    pipe(server, client);
    server.on('close', () => client.destroy());
    client.on('close', () => server.destroy());
  })
  .listen(listen, '127.0.0.1', () => {
    console.log(`127.0.0.1:${listen} -> ${host}:${port}, ${oneWay * 2} ms round trip${mbps ? `, ${mbps} Mbit/s` : ''}${jitter ? `, up to ${jitter} ms jitter` : ''}`);
  });
