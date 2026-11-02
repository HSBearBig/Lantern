// @vitest-environment node
import { webcrypto } from 'node:crypto';
import { beforeAll, describe, expect, it } from 'vitest';
import { b64url } from './codec';
import { generateIdentity, hmac, publicKeyId, sign, verifyHmac, verifyHost } from './crypto';

beforeAll(() => { Object.defineProperty(globalThis, 'crypto', { value: webcrypto, configurable: true }); });

describe('cryptographic transcripts', () => {
  it('uses canonical array payloads for HMAC proofs', async () => {
    const secret = b64url(webcrypto.getRandomValues(new Uint8Array(32)));
    const values = ['lantern-pair-v1', 'host', 'client', 'key', 'nonce'];
    const proof = await hmac(secret, values);
    expect(await verifyHmac(secret, values, proof)).toBe(true);
    expect(await verifyHmac(secret, [...values, 'changed'], proof)).toBe(false);
  });

  it('binds signatures to every answer transcript field', async () => {
    const identity = await generateIdentity();
    expect(await publicKeyId(identity.publicKey)).toBe(identity.clientId);
    const transcript = ['lantern-answer-v1', 'host', identity.clientId, 'nonce', 'offer', 'answer'];
    const signature = await sign(identity, transcript);
    expect(await verifyHost(identity.publicKey, transcript, signature)).toBe(true);
    expect(await verifyHost(identity.publicKey, [...transcript.slice(0, -1), 'substituted'], signature)).toBe(false);
  });
});

import vector from '../../../crates/host/tests/fixtures/webcrypto-vector.json';
it('verifies the shared WebCrypto/Rust transcript fixture including Unicode and CRLF', async () => {
  expect(await verifyHost(vector.hostKey, vector.parts, vector.signature)).toBe(true);
  expect(await hmac(vector.secret, vector.parts)).toBe(vector.proof);
});
