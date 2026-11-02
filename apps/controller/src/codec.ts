const encoder = new TextEncoder();

export const utf8 = (value: string): Uint8Array<ArrayBuffer> => Uint8Array.from(encoder.encode(value));

export function b64url(bytes: BufferSource): string {
  const data = bytes instanceof ArrayBuffer
    ? new Uint8Array(bytes)
    : new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let binary = '';
  for (const value of data) binary += String.fromCharCode(value);
  return btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
}

export function fromB64url(value: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]+$/.test(value)) throw new Error('無效的 Base64URL');
  const padded = value.replaceAll('-', '+').replaceAll('_', '/') + '='.repeat((4 - value.length % 4) % 4);
  const binary = atob(padded);
  return Uint8Array.from(binary, char => char.charCodeAt(0));
}

export function hex(bytes: BufferSource): string {
  const data = bytes instanceof ArrayBuffer
    ? new Uint8Array(bytes)
    : new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return [...data].map(value => value.toString(16).padStart(2, '0')).join('');
}

export const canonical = (values: unknown[]): string => JSON.stringify(values);
