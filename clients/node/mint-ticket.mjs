// Reference ticket minter for host apps. No dependencies: Node's crypto only.
//
//   import { mintTicket } from './mint-ticket.mjs';
//   const key = loadPrivateKey('/path/to/ticket-private.pem');
//   const ticket = mintTicket(key, { aud: 'my-desktop', sub: 'alice', scope: ['view'] });
//
// CLI (for tests and the interop vector):
//   node mint-ticket.mjs --key <pem> --aud my-desktop --sub alice --scope view,input [--ttl 60]

import { createPrivateKey, randomBytes, sign } from 'node:crypto';
import { readFileSync } from 'node:fs';

export const SCOPES = ['view', 'input', 'clipboard', 'audio'];

export function loadPrivateKey(path) {
  return createPrivateKey(readFileSync(path));
}

/**
 * Mint a single-use ticket. `ttl` is at most 120 seconds; the server rejects longer ones.
 * @param {import('node:crypto').KeyObject} key Ed25519 private key
 * @param {{aud: string, sub: string, scope?: string[], ttl?: number, mon?: string, ses?: number,
 *          now?: number, jti?: string}} opts
 */
export function mintTicket(key, opts) {
  const now = opts.now ?? Math.floor(Date.now() / 1000);
  const ttl = Math.min(opts.ttl ?? 60, 120);
  const scope = opts.scope ?? ['view'];
  for (const s of scope) {
    if (!SCOPES.includes(s)) throw new Error(`unknown scope ${s}`);
  }
  const claims = {
    v: 1,
    aud: opts.aud,
    sub: opts.sub,
    iat: now,
    exp: now + ttl,
    jti: opts.jti ?? randomBytes(18).toString('base64url'),
    scope,
  };
  if (opts.mon) claims.mon = opts.mon;
  if (opts.ses) claims.ses = opts.ses;
  const input = `sk1.${Buffer.from(JSON.stringify(claims)).toString('base64url')}`;
  const sig = sign(null, Buffer.from(input), key).toString('base64url');
  return `${input}.${sig}`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), []),
  );
  const key = loadPrivateKey(args.key);
  process.stdout.write(
    mintTicket(key, {
      aud: args.aud,
      sub: args.sub ?? 'user',
      scope: (args.scope ?? 'view').split(','),
      ttl: args.ttl ? Number(args.ttl) : undefined,
      now: args.now ? Number(args.now) : undefined,
      jti: args.jti,
    }),
  );
}
