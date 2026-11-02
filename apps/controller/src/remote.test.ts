import { describe, expect, it, vi } from 'vitest';
import { RemoteSession, sequenceControlEvent, validEvent } from './remote';

describe('control event validation', () => {
  it('rejects out-of-range coordinates and dangerous text', () => {
    expect(validEvent({ type: 'pointer', x: .5, y: 1 })).toBe(true);
    expect(validEvent({ type: 'pointer', x: Number.NaN, y: .5 })).toBe(false);
    expect(validEvent({ type: 'pointer', x: 1.01, y: .5 })).toBe(false);
    expect(validEvent({ type: 'text', text: '中文輸入' })).toBe(true);
    expect(validEvent({ type: 'text', text: 'bad\0text' })).toBe(false);
  });
});

describe('cross-channel input ordering', () => {
  it('gives pointer and button events one monotonic sequence and puts coordinates on clicks', () => {
    const order = { seq: 0, x: 0, y: 0 };
    expect(sequenceControlEvent({ type: 'pointer', x: .25, y: .75 }, order)).toEqual({ type: 'pointer', x: .25, y: .75, seq: 1 });
    expect(sequenceControlEvent({ type: 'button', button: 'left', down: true }, order)).toEqual({ type: 'button', button: 'left', down: true, x: .25, y: .75, seq: 2 });
    expect(sequenceControlEvent({ type: 'button', button: 'left', down: false }, order)).toEqual({ type: 'button', button: 'left', down: false, x: .25, y: .75, seq: 3 });
  });

  it('disconnects instead of silently dropping reliable events under backpressure', () => {
    const states: string[] = [];
    const channel = { readyState: 'open', bufferedAmount: 300 * 1024, send: vi.fn(), close: vi.fn() };
    const session = new RemoteSession({} as never, { srcObject: null } as never, state => states.push(state), vi.fn());
    Object.assign(session as unknown as Record<string, unknown>, { control: channel });
    expect(session.send({ type: 'key', code: 'KeyA', down: false })).toBe(false);
    expect(states).toEqual(['error']);
    expect(channel.send).not.toHaveBeenCalled();
    expect(channel.close).toHaveBeenCalled();
  });
});

describe('bounded authentication', () => {
  it('times out a missing host and cancels stale timeouts after stop', async () => {
    vi.useFakeTimers();
    const states: string[] = [];
    const signaling = { open: vi.fn(async () => undefined), subscribe: vi.fn(() => vi.fn()), onClose: vi.fn(() => vi.fn()), send: vi.fn() };
    const session = new RemoteSession(signaling as never, { srcObject: null } as never, state => states.push(state), vi.fn());
    const device = { hostId: 'a'.repeat(64), hostKey: 'key', name: 'host', addedAt: 0 };
    await session.connect(device, { clientId: 'client', publicKey: 'key', keyPair: {} as CryptoKeyPair });
    expect(states).toEqual(['signaling', 'authenticating']);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(states.at(-1)).toBe('error');

    states.length = 0;
    await session.connect(device, { clientId: 'client', publicKey: 'key', keyPair: {} as CryptoKeyPair });
    session.stop(false);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(states).toEqual(['signaling', 'authenticating']);
    vi.useRealTimers();
  });
});
