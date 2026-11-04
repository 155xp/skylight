# Embedding Skylight in your own app

Skylight has no login of its own. Your web app decides who may see or control the desktop, then
hands the browser a short-lived ticket signed with a key only your app holds. Skylight checks
the signature and opens the session. This page covers the four pieces: keys, the ticket
endpoint, the browser component and the network path.

## 1. Keys

Give your app its own signing key and let Skylight trust its public half:

```sh
skylight keygen --out-dir ~/.config/my-app/skylight
# ~/.config/my-app/skylight/ticket-private.pem   (chmod 600, read by your app's server only)
# ~/.config/my-app/skylight/ticket-public.txt
```

Skylight can trust several keys at once (comma-separated), so you can keep its own key for the
built-in viewer next to your app's:

```sh
# ~/.config/skylight/env
SKYLIGHT_TICKET_KEY=/home/you/.config/skylight/ticket-public.txt,/home/you/.config/my-app/skylight/ticket-public.txt
SKYLIGHT_AUDIENCE=my-desktop
SKYLIGHT_ALLOW_ORIGINS=https://app.example.com
```

then `systemctl --user restart skylight`. `SKYLIGHT_AUDIENCE` defaults to the machine's hostname;
every ticket must carry the same value as `aud`. `SKYLIGHT_ALLOW_ORIGINS` lists the origins
whose pages may open Skylight's WebSockets (the built-in viewer is always allowed).

## 2. The ticket endpoint

Mint one ticket per connection, only for a signed-in user, from a `POST` route that checks the
`Origin` header. This is `clients/node/mint-ticket.mjs` inlined; it only needs `node:crypto`:

```js
import { createPrivateKey, randomBytes, sign } from 'node:crypto';
import { readFileSync } from 'node:fs';

const skylightKey = createPrivateKey(readFileSync(process.env.SKYLIGHT_SIGNING_KEY));

function mintSkylightTicket(user, scope) {
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    v: 1,
    aud: 'my-desktop',                // Skylight's SKYLIGHT_AUDIENCE
    sub: user,                        // shows up in Skylight's logs
    iat: now,
    exp: now + 30,                    // Skylight refuses anything over 120 s
    jti: randomBytes(18).toString('base64url'),
    scope,                            // 'view' always; add 'input', 'clipboard' for control
    ses: 4 * 60 * 60,                 // optional: longest session this ticket may open
  };
  const input = `sk1.${Buffer.from(JSON.stringify(claims)).toString('base64url')}`;
  return `${input}.${sign(null, Buffer.from(input), skylightKey).toString('base64url')}`;
}

// POST /api/desk/ticket, after your own session and Origin checks:
res.json({
  ticket: mintSkylightTicket(session.user, ['view', 'input', 'clipboard']),
  urls: ['wss://desk.example.com/ws'],   // tried in order, see "Network path" below
});
```

To offer a view-only screen, mint `['view']`: Skylight then refuses all input for that session
on the server, whatever the page does. `mon` (a connector name such as `HDMI-A-1`) pins a ticket
to one display. The full claim format is in [docs/PROTOCOL.md](docs/PROTOCOL.md) and
`src/ticket.rs`.

## 3. The browser component

Copy `web/skylight.js` into your app (it is one self-contained ES module) and mount it:

```js
import { Skylight } from './skylight.js';

async function getTicket() {
  const res = await fetch('/api/desk/ticket', { method: 'POST', credentials: 'same-origin', cache: 'no-store' });
  if (!res.ok) throw new Error(`ticket ${res.status}`);
  return res.json();
}

const first = await getTicket();
let spare = first.ticket;
const view = new Skylight(document.querySelector('#desk'), {
  url: first.urls,
  // A fresh ticket for every reconnect; the first one is already in hand.
  ticket: async () => (spare ? ((t) => ((spare = null), t))(spare) : (await getTicket()).ticket),
  reconnect: true,
});
view.addEventListener('state', (e) => console.log(e.detail.state));
view.addEventListener('stats', (e) => console.log(e.detail));
await view.connect();
```

The element Skylight mounts into should have a size (it fills it) and a black background. The
embed API, all optional except `url` and `ticket`:

| Option | Default | |
|---|---|---|
| `url` | | a `wss://` URL, or a list tried in order |
| `ticket` | | a string, or a function returning one (needed for reconnects) |
| `viewOnly` | `false` | ignore local input (the server enforces the ticket's scope anyway) |
| `codecs` | `['h264','hevc','av1']` | preference order |
| `maxFps` | display refresh | measured from `requestAnimationFrame` |
| `maxBitrate` | server cap | bits per second |
| `cmdKey` | `'ctrl'` | what Cmd sends on a Mac: `'ctrl'` (Cmd+C copies) or `'super'` |
| `touchMode` | `'trackpad'` | phones: relative pointer like a laptop trackpad, or `'direct'` taps |
| `scale` | `'fit'` | or `'native'` for one stream pixel per device pixel |
| `worker` | `true` | decode in a worker so the page's own work never delays a frame; `false` decodes on the page's thread |
| `smooth` | `false` | while frames arrive unevenly (a jittery tunnel), show at most one per display refresh: less judder, for up to a refresh more latency; frames that arrive evenly are drawn at once either way |

Methods: `connect()`, `disconnect()`, `setMonitor(id)`, `setOptions({...})`, `showKeyboard()`
(call from a tap, opens the iPhone keyboard), `typeText(s)`, `sendKeys([evdev codes])` (for
example `[29, 56, 111]` for Ctrl+Alt+Delete), `measureLatency({ mode: 'click' | 'key' | 'move' })`.
Events: `state` (`connecting`, `connected`, `disconnected`), `stats` (once a second), `monitors`,
`clipboard`, `error`, and `health`. The client checks once a second that frames reach the screen:
a browser decoder that holds frames back is flushed, and one that keeps doing it or draws a black
picture while the server's is not is replaced by a software decoder. Each problem fires `health`
with `detail.issue` (`decoder-stall`, `black-output`, `no-video`) and a sentence in
`detail.detail`; `issue: null` means it cleared. `detail.final` is true when Skylight has nothing
left to try (no picture 8 s after connecting, or still black with the software decoder): a page
with a second transport can switch to it then.

The module draws no chrome of its own, so your app keeps its own toolbar and frame.

### Content Security Policy

Add each Skylight host to `connect-src` (for example `connect-src 'self' wss://desk.example.com`).
Keep `data:` in `img-src`: Skylight turns the remote cursor into a `data:` PNG for the CSS cursor.
Allow `blob:` in `worker-src` (for example `worker-src 'self' blob:`): the video socket and decoder
run in a worker that skylight.js builds from its own code. Without it Skylight still works, but
decodes on the page's thread, where a busy page can delay frames.
WebCodecs needs a secure context, so the page must be served over HTTPS (or from localhost).

## 4. Network path

Skylight only listens on loopback. Something in front of it has to carry WebSockets to the
browser:

- **A reverse proxy on the same machine** that holds the TLS certificate. With Caddy:

  ```caddyfile
  desk.example.com {
      request_body {
          max_size 4MB
      }
      header X-Robots-Tag "noindex, nofollow, noarchive"
      reverse_proxy 127.0.0.1:5490 {
          flush_interval -1
      }
  }
  ```

  To serve it under a path of an existing site instead, wrap the same `reverse_proxy` in
  `handle_path /desk/*` and use `wss://example.com/desk/ws`.
- **A raw TCP tunnel to that proxy** works when the machine has no public address. The
  "public" figures in the README were measured this way: the tunnel's relay added about 47-61 ms
  of round trip, carried about 5 Mbit/s per connection, and reset some fresh TLS handshakes from
  Chromium. The client retries those with the same ticket, which is safe because a ticket is only
  sent once a socket is open.
- **A private network (WireGuard or a mesh VPN)** is the fastest secure option: peer to peer,
  no relay, reachable only from your own devices. It still needs an HTTPS certificate, because
  WebCodecs only runs on secure pages and an HTTPS page cannot open a `ws://` socket. Put its URL
  first in `urls`: off the private network the name does not resolve, that attempt fails at once,
  and the client falls back to the next URL with the same ticket.
- **Static hosts and serverless platforms** that proxy plain HTTP usually cannot carry WebSocket
  upgrades. Keep the page there if you like, and point the socket at one of the options above.

UDP transports (WebRTC with TURN, WebTransport) are not implemented: they need a UDP-capable path
and, over a direct private network, TCP is already close to the wire.

### Optional: an extra gate in front of the sockets

The ticket travels in the first WebSocket message, so a proxy cannot read it. If you want the
proxy itself to refuse upgrades from anyone who has not just asked your app for a ticket, have
the ticket endpoint also set a short-lived, `HttpOnly`, `Secure` cookie scoped to the socket path,
and put a `forward_auth` (Caddy) or `auth_request` (nginx) check on `/ws` and `/input` that
validates it. Skylight still authorizes every session by its ticket.

## 5. Checklist

1. `skylight keygen --out-dir <your app's key dir>`; add the public key to `SKYLIGHT_TICKET_KEY`
   and your app's origin to `SKYLIGHT_ALLOW_ORIGINS`, then restart the service.
2. Add the ticket endpoint behind your own login.
3. Copy `web/skylight.js` into the app and mount it as above; add the Skylight host to the CSP.
4. Put a TLS-terminating reverse proxy (or a private network) in front of `127.0.0.1:5490`.
5. Signed out, the ticket endpoint must refuse (401), and a socket that does not present a valid
   ticket within 5 seconds is closed.
6. Signed in: connect, check the stats, and run Settings, Measure from each device you use.
