import { b64url, canonical, fromB64url, hex, utf8 } from './codec';

export type BrowserIdentity = {
  keyPair: CryptoKeyPair;
  clientId: string;
  publicKey: string;
};

export async function generateIdentity(): Promise<BrowserIdentity> {
  const keyPair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']);
  return identityFromKeys(keyPair);
}

export async function identityFromKeys(keyPair: CryptoKeyPair): Promise<BrowserIdentity> {
  const raw = await crypto.subtle.exportKey('raw', keyPair.publicKey);
  const digest = await crypto.subtle.digest('SHA-256', raw);
  return { keyPair, clientId: hex(digest), publicKey: b64url(raw) };
}

export async function hmac(secret: string, values: unknown[]): Promise<string> {
  const key = await crypto.subtle.importKey('raw', fromB64url(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return b64url(await crypto.subtle.sign('HMAC', key, utf8(canonical(values))));
}

export async function verifyHmac(secret: string, values: unknown[], proof: string): Promise<boolean> {
  const key = await crypto.subtle.importKey('raw', fromB64url(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['verify']);
  return crypto.subtle.verify('HMAC', key, fromB64url(proof), utf8(canonical(values)));
}

export async function sign(identity: BrowserIdentity, values: unknown[]): Promise<string> {
  return b64url(await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, identity.keyPair.privateKey, utf8(canonical(values))));
}

export async function verifyHost(hostKey: string, values: unknown[], signature: string): Promise<boolean> {
  const key = await crypto.subtle.importKey('raw', fromB64url(hostKey), { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
  return crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, fromB64url(signature), utf8(canonical(values)));
}

export async function publicKeyId(publicKey: string): Promise<string> {
  return hex(await crypto.subtle.digest('SHA-256', fromB64url(publicKey)));
}

export function randomNonce(): string {
  return b64url(crypto.getRandomValues(new Uint8Array(32)));
}
