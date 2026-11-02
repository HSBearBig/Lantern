import { sign, verifyHost, type BrowserIdentity } from './crypto';
import { createPairRequest, verifyPairResponse, type PairingInvite } from './pairing';
import { SignalingClient } from './signaling';
import type { ControlEvent, Device, DisplayInfo, HostPayload } from './types';

export type SessionState = 'idle' | 'signaling' | 'authenticating' | 'connecting' | 'connected' | 'disconnected' | 'error';

export class RemoteSession {
  private pc?: RTCPeerConnection;
  private control?: RTCDataChannel;
  private pointer?: RTCDataChannel;
  private unsubscribe?: () => void;
  private unsubscribeClose?: () => void;
  private offerSdp = '';
  private challenge = '';
  private device?: Device;
  private identity?: BrowserIdentity;
  private stopped = false;
  private generation = 0;
  private timeout?: number;
  private inputOrder: InputOrder = { seq: 0, x: 0, y: 0 };

  constructor(
    private readonly signaling: SignalingClient,
    private readonly video: HTMLVideoElement,
    private readonly onState: (state: SessionState, detail?: string) => void,
    private readonly onDisplay: (display: DisplayInfo) => void,
  ) {}

  async pair(invite: PairingInvite, identity: BrowserIdentity, clientName: string): Promise<Device> {
    this.onState('signaling');
    await this.signaling.open();
    const request = await createPairRequest(invite, identity, clientName);
    return new Promise<Device>((resolve, reject) => {
      const timeout = window.setTimeout(() => { unsubscribe(); reject(new Error('配對逾時，請重新產生配對連結')); }, 15_000);
      const unsubscribe = this.signaling.subscribe(async (from, payload) => {
        if (from !== invite.hostId) return;
        if (payload.type === 'error') {
          window.clearTimeout(timeout); unsubscribe(); reject(new Error(payload.message)); return;
        }
        if (payload.type !== 'pair_ok') return;
        window.clearTimeout(timeout); unsubscribe();
        try {
          if (!await verifyPairResponse(invite, identity, payload, request.nonce)) throw new Error('主機配對證明驗證失敗');
          const { secret: _, ...device } = invite;
          resolve(device);
        } catch (error) { reject(error); }
      });
      try { this.signaling.send(invite.hostId, request); }
      catch (error) { window.clearTimeout(timeout); unsubscribe(); reject(error); }
    });
  }

  async connect(device: Device, identity: BrowserIdentity): Promise<void> {
    this.stop(false);
    this.stopped = false;
    const generation = this.generation;
    this.inputOrder = { seq: 0, x: 0, y: 0 };
    this.device = device;
    this.identity = identity;
    this.onState('signaling');
    await this.signaling.open();
    if (!this.isCurrent(generation)) return;
    this.unsubscribeClose = this.signaling.onClose(() => {
      if (this.isCurrent(generation) && this.pc?.connectionState !== 'connected') {
        this.onState('disconnected', '連線協調服務中斷'); this.stop(false);
      }
    });
    this.unsubscribe = this.signaling.subscribe((from, payload) => {
      if (from === device.hostId) void this.handlePayload(payload, generation);
    });
    this.onState('authenticating');
    this.signaling.send(device.hostId, { type: 'connect', clientId: identity.clientId });
    this.armTimeout(10_000, generation, '等待主機驗證逾時');
  }

  send(event: ControlEvent): boolean {
    if (!validEvent(event)) return false;
    if (this.stopped) return false;
    const outbound = sequenceControlEvent(event, this.inputOrder);
    const channel = outbound.type === 'pointer' ? this.pointer : this.control;
    if (channel?.readyState !== 'open') return false;
    if (channel === this.control && channel.bufferedAmount > 256 * 1024) {
      this.failReliable('控制通道壅塞，已中斷連線以釋放按鍵');
      return false;
    }
    if (channel === this.pointer && channel.bufferedAmount > 64 * 1024) return false;
    try { channel.send(JSON.stringify(outbound)); return true; }
    catch {
      if (channel === this.control) this.failReliable('控制通道傳送失敗');
      return false;
    }
  }

  stop(announce = true): void {
    this.stopped = true;
    this.generation = (this.generation + 1) >>> 0;
    this.clearTimeout();
    if (this.control?.readyState === 'open' && this.control.bufferedAmount <= 256 * 1024) {
      try { this.control.send(JSON.stringify({ type: 'release_all' } satisfies ControlEvent)); } catch { /* Closing the channel releases input host-side. */ }
    }
    this.unsubscribe?.();
    this.unsubscribeClose?.();
    this.unsubscribe = undefined;
    this.unsubscribeClose = undefined;
    this.control?.close();
    this.pointer?.close();
    this.pc?.close();
    this.control = undefined;
    this.pointer = undefined;
    this.pc = undefined;
    this.video.srcObject = null;
    if (announce) this.onState('disconnected');
  }

  private async handlePayload(payload: HostPayload, generation: number): Promise<void> {
    if (!this.isCurrent(generation)) return;
    try {
      if (payload.type === 'error') throw new Error(payload.message);
      if (payload.type === 'challenge') await this.createOffer(payload.nonce, generation);
      if (payload.type === 'answer') await this.acceptAnswer(payload, generation);
    } catch (error) {
      if (!this.isCurrent(generation)) return;
      this.onState('error', error instanceof Error ? error.message : '連線失敗');
      this.stop(false);
    }
  }

  private async createOffer(nonce: string, generation: number): Promise<void> {
    if (!this.device || !this.identity || this.pc) throw new Error('收到非預期的連線挑戰');
    this.clearTimeout();
    this.challenge = nonce;
    const pc = new RTCPeerConnection({ iceServers: [], bundlePolicy: 'max-bundle' });
    this.pc = pc;
    this.control = pc.createDataChannel('control', { ordered: true });
    this.pointer = pc.createDataChannel('pointer', { ordered: false, maxRetransmits: 0 });
    this.control.onopen = () => { if (this.isCurrent(generation)) this.maybeConnected(); };
    this.pointer.onopen = () => { if (this.isCurrent(generation)) this.maybeConnected(); };
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.ontrack = event => {
      if (!this.isCurrent(generation)) return;
      this.video.srcObject = event.streams[0] ?? new MediaStream([event.track]);
      void this.video.play();
    };
    pc.onconnectionstatechange = () => {
      if (!this.isCurrent(generation)) return;
      if (pc.connectionState === 'connected') this.maybeConnected();
      if (!this.stopped && ['failed', 'disconnected', 'closed'].includes(pc.connectionState)) this.onState('disconnected', '區網連線中斷');
    };
    this.onState('connecting');
    const offer = await pc.createOffer();
    if (!this.isCurrent(generation)) { pc.close(); return; }
    await pc.setLocalDescription(offer);
    await waitForIce(pc, 8_000);
    if (!this.isCurrent(generation)) { pc.close(); return; }
    this.offerSdp = pc.localDescription?.sdp ?? '';
    if (!this.offerSdp) throw new Error('瀏覽器未產生 WebRTC Offer');
    const signature = await sign(this.identity, ['lantern-offer-v1', this.device.hostId, this.identity.clientId, nonce, this.offerSdp]);
    if (!this.isCurrent(generation)) { pc.close(); return; }
    this.signaling.send(this.device.hostId, { type: 'offer', clientId: this.identity.clientId, nonce, sdp: this.offerSdp, signature });
    this.armTimeout(20_000, generation, '建立區網連線逾時');
  }

  private async acceptAnswer(payload: Extract<HostPayload, { type: 'answer' }>, generation: number): Promise<void> {
    if (!this.pc || !this.device || payload.nonce !== this.challenge) throw new Error('主機回應的挑戰值不符');
    const valid = await verifyHost(this.device.hostKey, ['lantern-answer-v1', this.device.hostId, this.identity!.clientId, payload.nonce, this.offerSdp, payload.sdp], payload.signature);
    if (!this.isCurrent(generation)) return;
    if (!valid || !payload.sdp.includes('a=fingerprint:sha-256 ')) throw new Error('主機 WebRTC 回應簽章或憑證指紋無效');
    this.onDisplay(payload.display);
    await this.pc.setRemoteDescription({ type: 'answer', sdp: payload.sdp });
  }

  private maybeConnected(): void {
    if (this.pc?.connectionState === 'connected' && this.control?.readyState === 'open' && this.pointer?.readyState === 'open') {
      this.clearTimeout();
      this.onState('connected');
    }
  }

  private armTimeout(delay: number, generation: number, message: string): void {
    this.clearTimeout();
    this.timeout = window.setTimeout(() => {
      if (!this.isCurrent(generation)) return;
      this.onState('error', message);
      this.stop(false);
    }, delay);
  }

  private clearTimeout(): void {
    if (this.timeout !== undefined) window.clearTimeout(this.timeout);
    this.timeout = undefined;
  }

  private isCurrent(generation: number): boolean { return !this.stopped && this.generation === generation; }

  private failReliable(message: string): void {
    if (this.stopped) return;
    this.onState('error', message);
    this.stop(false);
  }
}

export type InputOrder = { seq: number; x: number; y: number };

export function sequenceControlEvent(event: ControlEvent, order: InputOrder): ControlEvent {
  if (event.type !== 'pointer' && event.type !== 'button') return event;
  order.seq = (order.seq + 1) >>> 0;
  if (order.seq === 0) order.seq = 1;
  if (event.type === 'pointer') {
    order.x = event.x; order.y = event.y;
    return { ...event, seq: order.seq };
  }
  return { ...event, seq: order.seq, x: event.x ?? order.x, y: event.y ?? order.y };
}

async function waitForIce(pc: RTCPeerConnection, timeoutMs: number): Promise<void> {
  if (pc.iceGatheringState === 'complete') return;
  await new Promise<void>(resolve => {
    const timeout = window.setTimeout(done, timeoutMs);
    function done() { window.clearTimeout(timeout); pc.removeEventListener('icegatheringstatechange', changed); resolve(); }
    function changed() { if (pc.iceGatheringState === 'complete') done(); }
    pc.addEventListener('icegatheringstatechange', changed);
  });
}

export function validEvent(event: ControlEvent): boolean {
  if (event.type === 'pointer') return Number.isFinite(event.x) && Number.isFinite(event.y) && event.x >= 0 && event.x <= 1 && event.y >= 0 && event.y <= 1;
  if (event.type === 'button') return (event.x === undefined || Number.isFinite(event.x) && event.x >= 0 && event.x <= 1) && (event.y === undefined || Number.isFinite(event.y) && event.y >= 0 && event.y <= 1);
  if (event.type === 'wheel') return Number.isFinite(event.dx) && Number.isFinite(event.dy) && Math.abs(event.dx) <= 2000 && Math.abs(event.dy) <= 2000;
  if (event.type === 'key') return event.code.length > 0 && event.code.length <= 40;
  if (event.type === 'text') return new TextEncoder().encode(event.text).byteLength <= 16_384 && !event.text.includes('\0');
  return true;
}
