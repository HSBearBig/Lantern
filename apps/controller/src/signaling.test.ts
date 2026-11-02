import { afterEach, describe, expect, it, vi } from 'vitest';
import { SignalingClient } from './signaling';

class FakeSocket {
  static OPEN = 1;
  static instances: FakeSocket[] = [];
  readyState = 0;
  onmessage?: (event: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  send = vi.fn();
  constructor(_: string) { FakeSocket.instances.push(this); }
  close() { this.readyState = 3; this.onclose?.(); }
  register() { this.readyState = 1; this.onmessage?.({ data: JSON.stringify({ type: 'registered', id: 'socket' }) }); }
}

afterEach(() => { vi.unstubAllGlobals(); FakeSocket.instances = []; });
describe('signaling lifecycle', () => {
  it('shares one connection across simultaneous opens and surfaces closure', async () => {
    vi.stubGlobal('WebSocket', FakeSocket);
    const client = new SignalingClient('ws://localhost/ws');
    const closed = vi.fn(); client.onClose(closed);
    const a = client.open(); const b = client.open();
    expect(FakeSocket.instances).toHaveLength(1);
    const socket = FakeSocket.instances[0]; socket.register(); await Promise.all([a, b]);
    client.send('host', { type: 'connect' }); expect(socket.send).toHaveBeenCalledTimes(1);
    socket.close(); expect(closed).toHaveBeenCalledTimes(1);
    expect(() => client.send('host', {})).toThrow();
  });
  it('rejects a socket closed before registration instead of leaving open pending', async () => {
    vi.stubGlobal('WebSocket', FakeSocket);
    const client = new SignalingClient('ws://localhost/ws'); const pending = client.open();
    FakeSocket.instances[0].close(); await expect(pending).rejects.toThrow('註冊前中斷');
  });
});
