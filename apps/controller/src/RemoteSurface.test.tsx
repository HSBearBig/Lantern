import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { createRef } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RemoteSurface } from './RemoteSurface';
import type { ControlEvent } from './types';
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe('RemoteSurface text submission', () => {
  it('submits composed Unicode text only through the explicit action', () => {
    const send = vi.fn((_event: ControlEvent) => true);
    render(<RemoteSurface videoRef={createRef()} display={{ id: 'main', width: 1920, height: 1080, scale: 2 }} connected send={send} />);
    const input = screen.getByLabelText('送出文字');
    fireEvent.compositionStart(input);
    fireEvent.change(input, { target: { value: '中文🦀' } });
    fireEvent.compositionEnd(input);
    expect(send).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: '送出文字' }));
    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith({ type: 'text', text: '中文🦀' });
  });
});

describe('touch lifetime', () => {
  const prepare = async () => {
    class TestPointerEvent extends MouseEvent {
      pointerId: number;
      pointerType: string;
      constructor(type: string, options: MouseEventInit & { pointerId?: number; pointerType?: string } = {}) {
        super(type, options); this.pointerId = options.pointerId ?? 1; this.pointerType = options.pointerType ?? 'touch';
      }
    }
    vi.stubGlobal('PointerEvent', TestPointerEvent);
    const send = vi.fn((_event: ControlEvent) => true);
    render(<RemoteSurface videoRef={createRef()} display={{ id: 'main', width: 100, height: 100, scale: 1 }} connected send={send} />);
    const surface = screen.getByTestId('remote-surface');
    Object.defineProperty(surface, 'setPointerCapture', { value: vi.fn(), configurable: true });
    vi.spyOn(surface, 'getBoundingClientRect').mockReturnValue({ x: 0, y: 0, left: 0, top: 0, right: 100, bottom: 100, width: 100, height: 100, toJSON: () => ({}) });
    return { send, surface };
  };
  it('retains a drag longer than the long-press threshold and releases on lift', async () => {
    vi.useFakeTimers();
    const { send, surface } = await prepare();
    fireEvent.click(screen.getByRole('button', { name: '拖曳' }));
    fireEvent.pointerDown(surface, { pointerId: 1, pointerType: 'touch', clientX: 20, clientY: 30 });
    await vi.advanceTimersByTimeAsync(700);
    fireEvent.pointerUp(surface, { pointerId: 1, pointerType: 'touch', clientX: 20, clientY: 30 });
    expect(send).toHaveBeenCalledWith({ type: 'button', button: 'left', down: true });
    expect(send).toHaveBeenCalledWith({ type: 'button', button: 'left', down: false });
    expect(send).not.toHaveBeenCalledWith({ type: 'button', button: 'right', down: true });
    vi.useRealTimers(); vi.unstubAllGlobals();
  });
  it('cancels touch without creating a click and ignores letterbox taps', async () => {
    const { send, surface } = await prepare();
    fireEvent.pointerDown(surface, { pointerId: 1, pointerType: 'touch', clientX: 20, clientY: 30 });
    fireEvent.pointerCancel(surface, { pointerId: 1, pointerType: 'touch', clientX: 20, clientY: 30 });
    fireEvent.pointerUp(surface, { pointerId: 1, pointerType: 'touch', clientX: 20, clientY: 30 });
    expect(send).toHaveBeenCalledWith({ type: 'release_all' });
    expect(send.mock.calls.some(([event]) => (event as { type: string }).type === 'button')).toBe(false);
    send.mockClear();
    fireEvent.pointerDown(surface, { pointerId: 2, pointerType: 'touch', clientX: -10, clientY: 30 });
    fireEvent.pointerUp(surface, { pointerId: 2, pointerType: 'touch', clientX: -10, clientY: 30 });
    expect(send).not.toHaveBeenCalled();
    vi.unstubAllGlobals();
  });
});
