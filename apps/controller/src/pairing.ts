import { fromB64url } from './codec';
import { hmac, publicKeyId, randomNonce, verifyHmac, type BrowserIdentity } from './crypto';
import type { Device } from './types';

export type PairingInvite = Device & { secret: string };

export async function parseInvite(hash: string): Promise<PairingInvite | null> {
  const values = new URLSearchParams(hash.replace(/^#/, ''));
  if (!values.has('host')) return null;
  const hostId = values.get('host') ?? '';
  const hostKey = values.get('key') ?? '';
  const secret = values.get('secret') ?? '';
  if (!/^[a-f0-9]{64}$/.test(hostId)) throw new Error('配對連結的主機 ID 無效');
  if (fromB64url(secret).byteLength !== 32) throw new Error('配對密鑰無效');
  if (fromB64url(hostKey).byteLength !== 65) throw new Error('主機公鑰無效');
  if (await publicKeyId(hostKey) !== hostId) throw new Error('主機 ID 與公鑰不一致');
  return { hostId, hostKey, secret, name: values.get('name')?.slice(0, 80) || '遠端電腦', addedAt: Date.now() };
}

export async function createPairRequest(invite: PairingInvite, identity: BrowserIdentity, name: string) {
  const nonce = randomNonce();
  return {
    type: 'pair' as const,
    clientId: identity.clientId,
    clientKey: identity.publicKey,
    name: name.slice(0, 80),
    nonce,
    proof: await hmac(invite.secret, ['lantern-pair-v1', invite.hostId, identity.clientId, identity.publicKey, nonce]),
  };
}

export async function verifyPairResponse(invite: PairingInvite, identity: BrowserIdentity, response: {
  hostId: string; hostKey: string; nonce: string; proof: string;
}, requestNonce: string): Promise<boolean> {
  if (response.hostId !== invite.hostId || response.hostKey !== invite.hostKey || response.nonce !== requestNonce) return false;
  return verifyHmac(invite.secret, ['lantern-pair-ok-v1', response.hostId, response.hostKey, identity.clientId, response.nonce], response.proof);
}
