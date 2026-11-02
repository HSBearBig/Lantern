import { invoke } from '@tauri-apps/api/core';
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart';
import QRCode from 'qrcode';
import { useCallback, useEffect, useMemo, useState } from 'react';
import type { HostStatus, Settings, StartRequest } from './types';

type ServiceState = { running: boolean; status: HostStatus | null };

const emptyStatus: HostStatus = {
  hostId: '', name: '', online: false, enabled: false, session: null, pairingUrl: null,
  pairingExpiresIn: 0, pairedBrowsers: [], error: null,
};

export default function App() {
  const [settings, setSettings] = useState<Settings>({ serverUrl: 'https://lantern.example.invalid', deviceName: 'My computer', autostart: false });
  const [allowLocalHttp, setAllowLocalHttp] = useState(false);
  const [service, setService] = useState<ServiceState>({ running: false, status: null });
  const [qr, setQr] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const status = service.status ?? emptyStatus;

  const refresh = useCallback(async () => {
    try { setService(await invoke<ServiceState>('service_state')); }
    catch (error) { setMessage(errorText(error)); }
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const loaded = await invoke<Settings>('load_settings');
        const actualAutostart = await isEnabled().catch(() => loaded.autostart);
        setSettings({ ...loaded, autostart: actualAutostart });
      } catch (error) { setMessage(errorText(error)); }
      await refresh();
    })();
    const timer = window.setInterval(() => void refresh(), 750);
    return () => window.clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    if (!status.pairingUrl) { setQr(''); return; }
    let cancelled = false;
    void QRCode.toDataURL(status.pairingUrl, { width: 260, margin: 2, errorCorrectionLevel: 'M', color: { dark: '#07100aff', light: '#ffffffff' } })
      .then(value => { if (!cancelled) setQr(value); })
      .catch(() => { if (!cancelled) setMessage('無法產生配對 QR Code'); });
    return () => { cancelled = true; };
  }, [status.pairingUrl]);

  const localHttp = useMemo(() => {
    try { const url = new URL(settings.serverUrl); return url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname); }
    catch { return false; }
  }, [settings.serverUrl]);

  const perform = async (action: () => Promise<unknown>) => {
    setBusy(true); setMessage('');
    try { await action(); await refresh(); }
    catch (error) { setMessage(errorText(error)); }
    finally { setBusy(false); }
  };

  const save = async (next = settings) => invoke('save_settings', { settings: next });

  const start = () => perform(async () => {
    await save();
    const request: StartRequest = { ...settings, allowInsecureLocalhost: allowLocalHttp };
    await invoke('start_service', { request });
  });

  const toggleAutostart = async (checked: boolean) => {
    const previous = settings;
    const next = { ...settings, autostart: checked };
    setSettings(next); setMessage('');
    try {
      if (checked) await enable(); else await disable();
      await save(next);
    } catch (error) { setSettings(previous); setMessage(errorText(error)); }
  };

  return <main>
    <header>
      <div className="brand"><span>L</span><div><h1>Lantern Host</h1><p>Stage A 診斷 App</p></div></div>
      <div className={`health ${status.error ? 'error' : service.running && status.online ? 'online' : ''}`}>
        <i />{!service.running ? '服務未啟動' : status.error ? '需要處理' : status.online ? '協調服務已連線' : '正在連線'}
      </div>
    </header>

    {message && <div className="banner error">{message}</div>}
    {status.error && <div className="banner error">主機服務：{status.error}</div>}

    <div className="layout">
      <section className="card setup">
        <div className="section-title"><div><h2>主機服務</h2><p>首次啟動前請確認協調服務網址與裝置名稱。</p></div><span className="prototype">技術原型</span></div>
        <label>HTTPS 網站／協調服務
          <input value={settings.serverUrl} disabled={service.running} spellCheck={false} onChange={event => setSettings({ ...settings, serverUrl: event.target.value.trim() })} />
        </label>
        {localHttp && <label className="check warning"><input type="checkbox" checked={allowLocalHttp} disabled={service.running} onChange={event => setAllowLocalHttp(event.target.checked)} />允許 localhost HTTP 開發模式</label>}
        <label>裝置名稱
          <input value={settings.deviceName} disabled={service.running} maxLength={100} onChange={event => setSettings({ ...settings, deviceName: event.target.value })} />
        </label>
        <label className="check"><input type="checkbox" checked={settings.autostart} onChange={event => void toggleAutostart(event.target.checked)} />登入後啟動 Lantern 與主機服務</label>
        <div className="actions">
          {!service.running
            ? <button className="primary" disabled={busy || !settings.deviceName || !settings.serverUrl} onClick={() => void start()}>啟動服務</button>
            : <button className="danger" disabled={busy} onClick={() => void perform(() => invoke('stop_service'))}>停止服務</button>}
          {service.running && <button disabled={busy} onClick={() => void perform(() => invoke('set_enabled', { enabled: !status.enabled }))}>{status.enabled ? '暫停接受連線' : '恢復接受連線'}</button>}
          {status.session && <button disabled={busy} onClick={() => void perform(() => invoke('disconnect_session'))}>中斷目前連線</button>}
        </div>
        <dl>
          <div><dt>主機 ID</dt><dd>{status.hostId ? status.hostId.slice(0, 20) + '…' : '服務啟動後產生'}</dd></div>
          <div><dt>遠端工作階段</dt><dd>{status.session ? `${status.session} 正在控制` : '無'}</dd></div>
          <div><dt>接受連線</dt><dd>{service.running ? status.enabled ? '是' : '已暫停' : '否'}</dd></div>
        </dl>
      </section>

      <section className="card pairing">
        <div className="section-title"><div><h2>配對瀏覽器</h2><p>連結只在五分鐘內有效，成功使用一次後即失效。</p></div></div>
        {!status.pairingUrl && <div className="pair-empty"><span>⌁</span><p>服務上線後產生一次性 QR Code。</p><button disabled={!service.running || !status.online || !status.enabled || busy} onClick={() => void perform(() => invoke('create_pairing'))}>產生配對 QR Code</button></div>}
        {status.pairingUrl && <div className="pair-code">
          {qr ? <img src={qr} alt="一次性瀏覽器配對 QR Code" /> : <div className="qr-loading">產生 QR Code…</div>}
          <p>{status.pairingExpiresIn} 秒後失效</p>
          <button onClick={() => void perform(() => invoke('copy_text', { text: status.pairingUrl }))}>複製配對連結</button>
        </div>}
      </section>

      <section className="card paired">
        <div className="section-title"><div><h2>已配對瀏覽器</h2><p>撤銷後，該瀏覽器必須重新掃描 QR Code。</p></div><b>{status.pairedBrowsers.length}</b></div>
        {status.pairedBrowsers.length === 0 ? <p className="muted">尚未配對任何瀏覽器。</p> : <ul>{status.pairedBrowsers.map(browser => <li key={browser.id}>
          <div><strong>{browser.name}</strong><small>{browser.id.slice(0, 18)}…</small></div>
          <button className="text-danger" disabled={busy} onClick={() => void perform(() => invoke('revoke_browser', { id: browser.id }))}>撤銷</button>
        </li>)}</ul>}
      </section>

      <section className="card permissions">
        <h2>系統權限提示</h2>
        <p>真正建立遠端工作階段時，系統可能要求權限。Lantern 不會把按鍵內容或配對密鑰寫入診斷紀錄。</p>
        <ul>
          <li><strong>macOS</strong><span>請在「隱私權與安全性」允許螢幕錄製與輔助使用。變更後可能需要重新啟動 App。</span></li>
          <li><strong>Windows</strong><span>一般桌面不需額外授權；系統管理員視窗、安全桌面與登入畫面不在第一版範圍。</span></li>
        </ul>
      </section>
    </div>
  </main>;
}

function errorText(error: unknown): string {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return '操作失敗';
}
