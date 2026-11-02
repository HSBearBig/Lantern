// @vitest-environment node
import { webcrypto } from 'node:crypto';
import { beforeAll, describe, expect, it } from 'vitest';
import { b64url } from './codec';
import { generateIdentity } from './crypto';
import { createPairRequest, parseInvite, verifyPairResponse } from './pairing';

beforeAll(() => { Object.defineProperty(globalThis, 'crypto', { value: webcrypto, configurable: true }); });

describe('pairing invitation', () => {
  it('accepts an identity-bound host invitation', async () => {
    const host = await generateIdentity();
    const secret = b64url(webcrypto.getRandomValues(new Uint8Array(32)));
    const invite = await parseInvite(`#host=${host.clientId}&key=${host.publicKey}&secret=${secret}&name=Office%20Mac`);
    expect(invite).toMatchObject({ hostId: host.clientId, hostKey: host.publicKey, name: 'Office Mac' });
  });

  it('rejects a substituted host public key', async () => {
    const host = await generateIdentity();
    const other = await generateIdentity();
    const secret = b64url(webcrypto.getRandomValues(new Uint8Array(32)));
    await expect(parseInvite(`#host=${host.clientId}&key=${other.publicKey}&secret=${secret}`)).rejects.toThrow('不一致');
  });

  it('rejects a valid pairing acknowledgement replayed for another request nonce', async () => {
    const host = await generateIdentity();
    const browser = await generateIdentity();
    const secret = b64url(webcrypto.getRandomValues(new Uint8Array(32)));
    const invite = (await parseInvite(`#host=${host.clientId}&key=${host.publicKey}&secret=${secret}`))!;
    const request = await createPairRequest(invite, browser, 'test');
    const responseNonce = request.nonce;
    const { hmac } = await import('./crypto');
    const proof = await hmac(secret, ['lantern-pair-ok-v1', host.clientId, host.publicKey, browser.clientId, responseNonce]);
    const response = { hostId: host.clientId, hostKey: host.publicKey, nonce: responseNonce, proof };
    expect(await verifyPairResponse(invite, browser, response, request.nonce)).toBe(true);
    expect(await verifyPairResponse(invite, browser, response, 'another-nonce')).toBe(false);
  });
});
