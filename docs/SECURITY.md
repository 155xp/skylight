# Security and threat model

**What is at stake:** keyboard and mouse on a logged-in desktop session, which amounts to running
anything as that user, plus whatever is on screen and in the clipboard.

**Who might try:** anyone on the internet who finds the public URL; anyone on the LAN or the
private network; a web page open in the user's browser (cross-site WebSocket); someone watching
the network; a leaked ticket or a leaked Skylight config.

**What stops them:**

- **No open port.** Skylight refuses to bind anything but loopback (`127.0.0.1:5490` by default).
  It is reachable only through whatever you put in front of it: a reverse proxy, a tunnel to that
  proxy, or a private network.
- **Short-lived signed tickets, never passwords.** The format is
  `sk1.<base64url claims>.<Ed25519 signature>`, with claims `{v, aud, sub, iat, exp, jti, scope,
  mon?, ses?}`. Skylight verifies the signature against the public keys it trusts, requires its
  own `aud`, rejects anything that lives longer than 120 s or is expired or not yet valid (30 s
  skew), spends each `jti` once, and caps sessions at 24 h. Skylight holds only public keys, so
  its config cannot mint access.
- **Scopes are enforced on the server.** `view` is required. Without `input` there is no input
  socket and input messages are dropped; `clipboard` gates both directions. `mon` pins a ticket
  to one display: if that display goes away the session ends rather than show another one.
- **Tickets stay out of logs.** A ticket travels in the first WebSocket message, never in a URL a
  proxy would log. The viewer's link carries it in the fragment and strips it on load.
- **Origin allowlist** on both WebSocket upgrades, against cross-site WebSocket hijacking.
- **Limits:** 5 s for a new socket (on `/ws` or `/input`) to present a ticket or an input token,
  at most 32 such sockets at once across both, 4 sessions, 1 MB clipboard text (refused with an
  error above that), messages up to that plus 4 kB. A viewer that stops reading for 15 s is
  disconnected, so nothing queues up for it. Keys outside evdev 1-248 and the power, sleep and
  radio (WLAN, WWAN, RFKILL) keys are dropped. Every held key, button and touch is released when
  a session ends, and nothing is injected after that.
- **TLS** ends at your reverse proxy (or the private network); a TCP tunnel's relay sees only
  ciphertext. The hop to Skylight is loopback.
- **Static pages** are `no-store` with a strict CSP (`default-src 'none'`, scripts and styles
  from self, `frame-ancestors 'none'`).
- **The service is sandboxed:** `ProtectSystem=strict`, `NoNewPrivileges`, write access only to
  the runtime directory.

**What it does not protect against:** a compromised host app holding the private key (it can
mint tickets; that is its job), anyone with a shell as the desktop user, or someone who can
replace `~/.local/bin/skylight` (KWin trusts that path through the `.desktop` file; anyone who
can write it already runs as that user). Bandwidth-exhaustion DoS through a tunnel is out of
scope. The latency probe opens a window on the desktop, and only for an `input` session.
