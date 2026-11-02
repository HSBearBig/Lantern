import { useCallback, useEffect, useRef, useState } from 'react';
import { parseInvite, type PairingInvite } from './pairing';
import { RemoteSession, type SessionState } from './remote';
import { RemoteSurface } from './RemoteSurface';
import { SignalingClient } from './signaling';
import { loadDevices, loadIdentity, removeDevice, saveDevice } from './store';
import type { BrowserIdentity } from './crypto';
import type { ControlEvent, Device, DisplayInfo } from './types';
import './styles.css';

const stateLabels: Record<SessionState, string> = {
  idle: '尚未連線', signaling: '連接協調服務…', authenticating: '驗證裝置…', connecting: '建立區網連線…',
  connected: '已連線', disconnected: '連線已中斷', error: '連線失敗',
};

// Capture once before React StrictMode replays effects, and remove bearer material immediately.
const initialPairingHash = location.hash;
if (new URLSearchParams(initialPairingHash.replace(/^#/, '')).has('host')) {
  history.replaceState(null, '', `${location.pathname}${location.search}`);
}

export default function App() {
  const videoRef = useRef<HTMLVideoElement>(null);
  const signalingRef = useRef(new SignalingClient());
  const sessionRef = useRef<RemoteSession | undefined>(undefined);
  const identityRef = useRef<BrowserIdentity | undefined>(undefined);
  const retryTimer = useRef<number | undefined>(undefined);
  const retryDeadline = useRef(0);
  const activeRef = useRef<Device | undefined>(undefined);
  const pairingPromiseRef = useRef<Promise<Device> | undefined>(undefined);
  const [devices, setDevices] = useState<Device[]>([]);
  const [invite, setInvite] = useState<PairingInvite | undefined>(undefined);
  const [state, setState] = useState<SessionState>('idle');
  const [detail, setDetail] = useState('');
  const [display, setDisplay] = useState<DisplayInfo | undefined>(undefined);
  const [active, setActive] = useState<Device | undefined>(undefined);

  const connect = useCallback(async (device: Device, retry = false) => {
    if (!identityRef.current || !sessionRef.current) return;
    window.clearTimeout(retryTimer.current);
    if (!retry) retryDeadline.current = Date.now() + 30_000;
    activeRef.current = device; setActive(device); setDetail('');
    try { await sessionRef.current.connect(device, identityRef.current); }
    catch (error) {
      setState('error'); setDetail(error instanceof Error ? error.message : '無法建立連線');
      if (activeRef.current && Date.now() < retryDeadline.current) {
        retryTimer.current = window.setTimeout(() => void connect(activeRef.current!, true), 2_000);
      }
    }
  }, []);

  useEffect(() => {
    let disposed = false;
    const pairingHash = initialPairingHash;
    void (async () => {
      try {
        const [identity, stored, parsed] = await Promise.all([loadIdentity(), loadDevices(), parseInvite(pairingHash)]);
        if (disposed) return;
        identityRef.current = identity;
        setDevices(stored);
        if (parsed) {
          setInvite(parsed);
        }
      } catch (error) { setState('error'); setDetail(error instanceof Error ? error.message : '初始化失敗'); }
    })();
    return () => { disposed = true; };
  }, []);

  useEffect(() => {
    if (!videoRef.current) return;
    sessionRef.current = new RemoteSession(signalingRef.current, videoRef.current, (next, message) => {
      setState(next); setDetail(message ?? '');
      if (next === 'connected') {
        window.clearTimeout(retryTimer.current);
        retryDeadline.current = 0;
      }
      if ((next === 'disconnected' || next === 'error') && activeRef.current && Date.now() < retryDeadline.current) {
        window.clearTimeout(retryTimer.current);
        retryTimer.current = window.setTimeout(() => void connect(activeRef.current!, true), 2_000);
      } else if ((next === 'disconnected' || next === 'error') && activeRef.current && retryDeadline.current === 0) {
        retryDeadline.current = Date.now() + 30_000;
        retryTimer.current = window.setTimeout(() => void connect(activeRef.current!, true), 2_000);
      }
    }, setDisplay);
    const release = () => sessionRef.current?.send({ type: 'release_all' });
    const visibility = () => { if (document.hidden) release(); };
    window.addEventListener('blur', release);
    window.addEventListener('pagehide', release);
    document.addEventListener('visibilitychange', visibility);
    return () => {
      window.clearTimeout(retryTimer.current);
      release(); sessionRef.current?.stop(false); signalingRef.current.close();
      window.removeEventListener('blur', release); window.removeEventListener('pagehide', release);
      document.removeEventListener('visibilitychange', visibility);
    };
  }, [connect]);

  useEffect(() => {
    if (!invite || !identityRef.current || !sessionRef.current) return;
    let cancelled = false;
    pairingPromiseRef.current ??= sessionRef.current.pair(invite, identityRef.current, browserName());
    void (async () => {
      try {
        const device = await pairingPromiseRef.current!;
        if (cancelled) return;
        pairingPromiseRef.current = undefined;
        const next = await saveDevice(device);
        setDevices(next); setInvite(undefined);
        await connect(device);
      } catch (error) { if (!cancelled) { pairingPromiseRef.current = undefined; setState('error'); setDetail(error instanceof Error ? error.message : '配對失敗'); } }
    })();
    return () => { cancelled = true; };
  }, [invite, connect]);

  const send = useCallback((event: ControlEvent) => sessionRef.current?.send(event) ?? false, []);
  const disconnect = () => { retryDeadline.current = 0; activeRef.current = undefined; sessionRef.current?.stop(); setActive(undefined); };

  return <main>
    <header>
      <div><span className="brand-mark">L</span><h1>Lantern</h1><span className="diagnostic">技術原型</span></div>
      <div className={`status ${state}`}><span />{stateLabels[state]}{detail && ` · ${detail}`}</div>
    </header>

    {invite && <section className="notice"><strong>正在配對 {invite.name}</strong><p>請保持被控端 App 開啟。配對連結只會在此瀏覽器記憶一次。</p></section>}

    <section className="workspace">
      <aside>
        <h2>已配對電腦</h2>
        {devices.length === 0 && <p className="empty">請用被控端 App 顯示的 QR Code 開啟這個網站。</p>}
        <ul>{devices.map(device => <li key={device.hostId} className={active?.hostId === device.hostId ? 'selected' : ''}>
          <button className="device" onClick={() => void connect(device)}><span className="computer">▣</span><span><strong>{device.name}</strong><small>{device.hostId.slice(0, 12)}</small></span></button>
          <button className="remove" aria-label={`移除 ${device.name}`} onClick={async () => {
            if (activeRef.current?.hostId === device.hostId) disconnect();
            setDevices(await removeDevice(device.hostId));
          }}>×</button>
        </li>)}</ul>
        {active && <button className="danger" onClick={disconnect}>停止遠端控制</button>}
        <div className="help"><strong>區網診斷提示</strong><p>控制裝置與被控端需在同一 Wi-Fi／LAN。若無法連線，請檢查 AP 隔離、防火牆與 Safari 本機網路權限。</p></div>
      </aside>
      <section className="viewer">
        <RemoteSurface videoRef={videoRef} display={display} connected={state === 'connected'} send={send} />
        <p className="gesture-help">iPad：點按＝左鍵、長按＝右鍵、雙指移動＝捲動；啟用「拖曳」後以單指拖曳。</p>
      </section>
    </section>
  </main>;
}

function browserName(): string {
  if (/iPad|iPhone/.test(navigator.userAgent)) return 'iPad Safari';
  return `瀏覽器 · ${navigator.platform || '未知裝置'}`.slice(0, 80);
}
