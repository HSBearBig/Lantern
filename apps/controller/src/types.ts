export type Device = {
  hostId: string;
  hostKey: string;
  name: string;
  addedAt: number;
};

export type DisplayInfo = import('../../../crates/protocol/bindings/DisplayInfo').DisplayInfo;
type WireControlEvent = import('../../../crates/protocol/bindings/ControlEvent').ControlEvent;
export type ControlEvent =
  | Exclude<WireControlEvent, { type: 'pointer' | 'button' }>
  | (Omit<Extract<WireControlEvent, { type: 'pointer' }>, 'seq'> & { seq?: number })
  | (Omit<Extract<WireControlEvent, { type: 'button' }>, 'seq' | 'x' | 'y'> & { seq?: number; x?: number; y?: number });

export type RelayEnvelope = { version: 1; to: string; from: string; payload: unknown };

export type HostPayload =
  | { type: 'pair_ok'; hostId: string; hostKey: string; nonce: string; proof: string }
  | { type: 'challenge'; nonce: string }
  | { type: 'answer'; nonce: string; sdp: string; signature: string; display: DisplayInfo }
  | { type: 'error'; code: string; message: string };
