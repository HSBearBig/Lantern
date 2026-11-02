import { generateIdentity, identityFromKeys, type BrowserIdentity } from './crypto';
import type { Device } from './types';

const DB = 'lantern-controller-v1';
const STORE = 'state';

function openDb(): Promise<IDBDatabase> {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open(DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function read<T>(key: string): Promise<T | undefined> {
  const db = await openDb();
  return new Promise<T | undefined>((resolve, reject) => {
    const request = db.transaction(STORE).objectStore(STORE).get(key);
    request.onsuccess = () => resolve(request.result as T | undefined);
    request.onerror = () => reject(request.error);
  }).finally(() => db.close());
}

async function write<T>(key: string, value: T): Promise<void> {
  const db = await openDb();
  return new Promise<void>((resolve, reject) => {
    const transaction = db.transaction(STORE, 'readwrite');
    transaction.objectStore(STORE).put(value, key);
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error);
  }).finally(() => db.close());
}

let identityPromise: Promise<BrowserIdentity> | undefined;

export function loadIdentity(): Promise<BrowserIdentity> {
  identityPromise ??= loadOrCreateIdentity();
  return identityPromise;
}

async function loadOrCreateIdentity(): Promise<BrowserIdentity> {
  let keyPair = await read<CryptoKeyPair>('identity');
  if (!keyPair) {
    const identity = await generateIdentity();
    await write('identity', identity.keyPair);
    return identity;
  }
  return identityFromKeys(keyPair);
}

export async function loadDevices(): Promise<Device[]> {
  return (await read<Device[]>('devices')) ?? [];
}

export async function saveDevice(device: Device): Promise<Device[]> {
  const devices = (await loadDevices()).filter(item => item.hostId !== device.hostId);
  devices.unshift(device);
  await write('devices', devices);
  return devices;
}

export async function removeDevice(hostId: string): Promise<Device[]> {
  const devices = (await loadDevices()).filter(item => item.hostId !== hostId);
  await write('devices', devices);
  return devices;
}
