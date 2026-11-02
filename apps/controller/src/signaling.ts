import type { HostPayload, RelayEnvelope } from './types';

type PayloadListener = (from: string, payload: HostPayload) => void;

export class SignalingClient {
  private socket?: WebSocket;
  private opening?: Promise<void>;
  private listeners = new Set<PayloadListener>();
  private closedListeners = new Set<() => void>();
  id = '';

  constructor(private readonly url = signalingUrl()) {}

  async open(): Promise<void> {
    if (this.opening) return this.opening;
    if (this.socket?.readyState === WebSocket.OPEN && this.id) return;
    this.opening = this.establish();
    try { await this.opening; } finally { this.opening = undefined; }
  }

  private establish(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(this.url);
      this.socket = socket;
      this.id = '';
      let registered = false;
      const timeout = window.setTimeout(() => {
        socket.close(); reject(new Error('連線協調服務逾時'));
      }, 10_000);
      socket.onmessage = event => {
        try {
          const message = JSON.parse(event.data as string) as Record<string, unknown>;
          if (message.type === 'registered' && typeof message.id === 'string') {
            this.id = message.id; registered = true;
            window.clearTimeout(timeout); resolve(); return;
          }
          const envelope = message as unknown as RelayEnvelope;
          if (envelope.version !== 1 || typeof envelope.from !== 'string' || !isHostPayload(envelope.payload)) return;
          for (const listener of this.listeners) listener(envelope.from, envelope.payload);
        } catch { /* Malformed relay messages are ignored. */ }
      };
      socket.onerror = () => {
        window.clearTimeout(timeout); socket.close(); reject(new Error('無法連上連線協調服務'));
      };
      socket.onclose = () => {
        window.clearTimeout(timeout);
        if (this.socket === socket) { this.socket = undefined; this.id = ''; }
        if (!registered) reject(new Error('連線協調服務在註冊前中斷'));
        for (const listener of this.closedListeners) listener();
      };
    });
  }

  subscribe(listener: PayloadListener): () => void {
    this.listeners.add(listener); return () => this.listeners.delete(listener);
  }
  onClose(listener: () => void): () => void {
    this.closedListeners.add(listener); return () => this.closedListeners.delete(listener);
  }
  send(to: string, payload: unknown): void {
    if (this.socket?.readyState !== WebSocket.OPEN || !this.id) throw new Error('連線協調服務尚未連線');
    this.socket.send(JSON.stringify({ version: 1, to, from: '', payload } satisfies RelayEnvelope));
  }
  close(): void { this.socket?.close(); this.socket = undefined; this.id = ''; }
}

function signalingUrl(): string {
  const configured = import.meta.env.VITE_SIGNALING_URL as string | undefined;
  if (configured) return configured;
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${location.host}/ws`;
}

function isHostPayload(value: unknown): value is HostPayload {
  if (!value || typeof value !== 'object') return false;
  const type = (value as { type?: unknown }).type;
  return type === 'pair_ok' || type === 'challenge' || type === 'answer' || type === 'error';
}
