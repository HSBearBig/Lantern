import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type WheelEvent } from 'react';
import { normalizedPoint } from './coordinates';
import type { ControlEvent, DisplayInfo } from './types';

type Props = {
  videoRef: React.RefObject<HTMLVideoElement | null>;
  display?: DisplayInfo;
  connected: boolean;
  send: (event: ControlEvent) => boolean;
};

type Touch = { x: number; y: number; startX: number; startY: number; started: number; rightTimer?: number; multi?: boolean };

export function RemoteSurface({ videoRef, display, connected, send }: Props) {
  const surfaceRef = useRef<HTMLDivElement>(null);
  const touches = useRef(new Map<number, Touch>());
  const [drag, setDrag] = useState(false);
  const [text, setText] = useState('');
  const [modifiers, setModifiers] = useState(new Set<string>());

  const point = (clientX: number, clientY: number) => {
    const bounds = surfaceRef.current?.getBoundingClientRect();
    if (!bounds || !display) return null;
    return normalizedPoint(clientX, clientY, bounds, display.width, display.height);
  };

  const click = (button: 'left' | 'right') => {
    send({ type: 'button', button, down: true });
    send({ type: 'button', button, down: false });
  };

  const onPointerDown = (event: ReactPointerEvent) => {
    if (!connected) return;
    const position = point(event.clientX, event.clientY);
    if (!position) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    send({ type: 'pointer', ...position });
    if (event.pointerType === 'mouse') {
      if (!position) return;
      send({ type: 'button', button: event.button === 2 ? 'right' : event.button === 1 ? 'middle' : 'left', down: true });
      return;
    }
    const touch: Touch = { x: event.clientX, y: event.clientY, startX: event.clientX, startY: event.clientY, started: performance.now() };
    touches.current.set(event.pointerId, touch);
    if (touches.current.size === 1) {
      if (!drag) touch.rightTimer = window.setTimeout(() => {
        if (touches.current.size === 1 && !drag) click('right');
        touches.current.delete(event.pointerId);
      }, 650);
      if (drag) send({ type: 'button', button: 'left', down: true });
    } else {
      for (const item of touches.current.values()) { item.multi = true; if (item.rightTimer) window.clearTimeout(item.rightTimer); }
      if (drag) send({ type: 'button', button: 'left', down: false });
    }
  };

  const onPointerMove = (event: ReactPointerEvent) => {
    if (!connected) return;
    if (event.pointerType === 'mouse') {
      const position = point(event.clientX, event.clientY);
      if (position) send({ type: 'pointer', ...position });
      return;
    }
    const touch = touches.current.get(event.pointerId);
    if (!touch) return;
    const dx = event.clientX - touch.x;
    const dy = event.clientY - touch.y;
    touch.x = event.clientX; touch.y = event.clientY;
    if (Math.hypot(event.clientX - touch.startX, event.clientY - touch.startY) > 8 && touch.rightTimer) window.clearTimeout(touch.rightTimer);
    if (touches.current.size >= 2) send({ type: 'wheel', dx: -dx * 2, dy: -dy * 2 });
    else {
      const position = point(event.clientX, event.clientY);
      if (position) send({ type: 'pointer', ...position });
    }
  };

  const onPointerUp = (event: ReactPointerEvent) => {
    if (event.pointerType === 'mouse') {
      send({ type: 'button', button: event.button === 2 ? 'right' : event.button === 1 ? 'middle' : 'left', down: false });
      return;
    }
    const touch = touches.current.get(event.pointerId);
    if (!touch) return;
    if (touch.rightTimer) window.clearTimeout(touch.rightTimer);
    const wasMultiTouch = touch.multi || touches.current.size > 1;
    touches.current.delete(event.pointerId);
    const distance = Math.hypot(event.clientX - touch.startX, event.clientY - touch.startY);
    if (drag) send({ type: 'button', button: 'left', down: false });
    else if (!wasMultiTouch && performance.now() - touch.started < 500 && distance < 8) click('left');
  };

  const onWheel = (event: WheelEvent) => {
    event.preventDefault();
    send({ type: 'wheel', dx: clampWheel(event.deltaX), dy: clampWheel(event.deltaY) });
  };

  const toggleModifier = (code: string) => {
    const down = !modifiers.has(code);
    send({ type: 'key', code, down });
    setModifiers(current => {
      const next = new Set(current);
      if (down) next.add(code); else next.delete(code);
      return next;
    });
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (!connected || event.isComposing || event.keyCode === 229 || target?.matches('input, textarea, button')) return;
      event.preventDefault();
      send({ type: 'key', code: event.code, down: event.type === 'keydown' });
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('keyup', onKey);
    return () => { window.removeEventListener('keydown', onKey); window.removeEventListener('keyup', onKey); };
  }, [connected, send]);

  useEffect(() => {
    const reset = () => {
      for (const touch of touches.current.values()) if (touch.rightTimer) window.clearTimeout(touch.rightTimer);
      touches.current.clear(); setModifiers(new Set());
    };
    if (!connected) { reset(); setDrag(false); }
    window.addEventListener('blur', reset);
    return () => { window.removeEventListener('blur', reset); reset(); };
  }, [connected]);

  return <>
    <div
      ref={surfaceRef}
      className="remote-surface"
      data-testid="remote-surface"
      onContextMenu={event => event.preventDefault()}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={() => {
        for (const touch of touches.current.values()) if (touch.rightTimer) window.clearTimeout(touch.rightTimer);
        touches.current.clear();
        send({ type: 'release_all' });
      }}
      onWheel={onWheel}
    >
      <video ref={videoRef} playsInline muted aria-label="遠端桌面畫面" />
      {!connected && <div className="video-placeholder">連線後會在這裡顯示遠端桌面</div>}
    </div>
    <div className="controls" aria-label="遠端操作工具列">
      {['ControlLeft', 'AltLeft', 'ShiftLeft', 'MetaLeft'].map(code =>
        <button key={code} className={modifiers.has(code) ? 'active' : ''} disabled={!connected} onClick={() => toggleModifier(code)}>
          {code.replace('Left', '').replace('Control', 'Ctrl').replace('Meta', '⌘')}
        </button>)}
      <button className={drag ? 'active' : ''} disabled={!connected} onClick={() => setDrag(value => !value)}>拖曳</button>
      <form className="text-send" onSubmit={event => {
        event.preventDefault();
        if (text && send({ type: 'text', text })) setText('');
      }}>
        <input aria-label="送出文字" value={text} maxLength={16_384} onChange={event => setText(event.target.value)} placeholder="輸入中文或文字" disabled={!connected} />
        <button disabled={!connected || !text}>送出文字</button>
      </form>
    </div>
  </>;
}

const clampWheel = (value: number) => Math.max(-2000, Math.min(2000, value));
