// Checks that mint-ticket.mjs still produces the exact ticket the Rust tests verify
// (src/ticket.rs, node_minted_ticket_verifies). Ed25519 is deterministic, so any change in the
// claim layout or encoding shows up here. Run: node --test clients/node/test/
import { createPrivateKey } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mintTicket } from '../mint-ticket.mjs';

// Test-only key: seed bytes 0x01..0x20. Never use it for anything real.
const seed = Buffer.from(Array.from({ length: 32 }, (_, i) => i + 1));
const der = Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed]);
const key = createPrivateKey({ key: der, format: 'der', type: 'pkcs8' });

test('ticket matches the Rust interop vector', () => {
  const expected = readFileSync(new URL('../testdata/ticket.txt', import.meta.url), 'utf8').trim();
  const t = mintTicket(key, {
    aud: 'my-desktop',
    sub: 'alice',
    scope: ['view', 'input'],
    now: 1762140000,
    jti: 'interop-test-vector-0001',
  });
  assert.equal(t, expected);
});

test('ttl is capped at 120 seconds', () => {
  const t = mintTicket(key, { aud: 'a', sub: 'b', ttl: 3600, now: 0 });
  const claims = JSON.parse(Buffer.from(t.split('.')[1], 'base64url'));
  assert.equal(claims.exp - claims.iat, 120);
});

test('unknown scopes are refused', () => {
  assert.throws(() => mintTicket(key, { aud: 'a', sub: 'b', scope: ['root'] }));
});
