# Endpoint and transport contracts

Protocol version is `1`. Generated TypeScript wire types live in `crates/protocol/bindings`; regenerate with `cargo test -p lantern-protocol`. The browser's input helper permits omitted sequence/coordinates, but RemoteSession always fills them before sending. The native host rejects button messages without an explicit sequence and position.

## Relay

`GET /healthz` returns status/protocol. `GET /ws` upgrades a WebSocket. Browser Origin must match `LANTERN_ORIGIN` exactly; native clients omit Origin. The relay bounds messages (128 KiB), per-socket messages (120/s), queued messages (64), concurrent connections and per-IP connections. It stores only live routing state in memory.

1. Relay sends `{type:"registered",id,nonce}`.
2. A native host claims a stable alias with `{type:"register_host",key,signature}`. The relay verifies an ECDSA P-256/SHA-256 signature over JSON `['lantern-register-v1',id,nonce]`. Its stable ID is the lowercase hex SHA-256 of the raw uncompressed SEC1 public key.
3. Relay acknowledges `{type:"host_registered",id:stableHostId}`. A duplicate live alias is rejected.
4. Route `{version:1,to,from:"",payload}`. Relay overwrites `from` using the authenticated alias or its own socket ID; callers cannot choose it. Unknown targets receive an `offline` error envelope.

All binary authentication fields use base64url without padding. ECDSA signatures use WebCrypto-compatible 64-byte IEEE P1363 `r || s`, not DER. Canonical signed data is UTF-8 encoding of the specified JSON string arrays. Shared test vectors cover Rust/WebCrypto compatibility.

## Pairing

The host creates 32 random bytes and a five-minute, single-use invitation. The URL fragment contains `host`, `key`, `secret`, `name`; HTTPS requests do not include that fragment. The controller removes it from history before async storage/crypto work, validates the host ID/key binding and keeps the secret in memory only.

Browser sends:

- `type: pair`, `clientId`, `clientKey`, `name`, random `nonce`, `proof`.
- `proof = HMAC-SHA256(secret, JSON(['lantern-pair-v1',hostId,clientId,clientKey,nonce]))`.

After persisting the authorized browser key, host consumes the invite and returns `pair_ok`, `hostId`, `hostKey`, the same `nonce`, and:

- `proof = HMAC-SHA256(secret, JSON(['lantern-pair-ok-v1',hostId,hostKey,clientId,nonce]))`.

Browser requires exact nonce and pinned-key equality, verifies HMAC, and stores the paired device. Browser private keys are non-extractable IndexedDB CryptoKeys; native identity uses the OS credential store. Clearing or failing storage does not silently restore trust. Corrupt host trust files fail closed.

## Reconnection and WebRTC

`connect(clientId)` obtains a random challenge tied to the live relay socket, expiring after 30 seconds. Unknown/revoked clients and concurrent sessions are rejected. The browser gathers LAN ICE candidates with no STUN/TURN and sends an SDP offer signed over:

`['lantern-offer-v1',hostId,clientId,nonce,offerSdp]`

The host consumes the challenge before checking the signature; a failed or replayed attempt requires a new challenge. Capture/input start only after successful authentication. Host returns display metadata and a signed SDP answer:

`['lantern-answer-v1',hostId,clientId,nonce,offerSdp,answerSdp]`

Both SDPs must contain SHA-256 DTLS fingerprints. The browser verifies the pinned host signature before accepting the answer. Media and control use standard encrypted WebRTC transports; the relay sees signaling metadata but never transports media or control events.

## Media and input

H.264 Baseline, packetization mode 1, no B-frame reordering. Native encoders emit Annex-B access units with SPS/PPS on IDR; RTCP PLI/FIR forces a keyframe. Capacity-one raw/encoded mailboxes prevent unbounded queueing; an encoded drop forces decoder recovery with a new IDR. Capture, encode and input run on one native worker thread so COM/WinRT ownership is preserved.

- `pointer`: unordered DataChannel, zero retransmits, normalized x/y plus monotonic sequence.
- `control`: reliable ordered DataChannel for buttons (including exact x/y and sequence), wheels, DOM key codes, committed Unicode text and release-all.
- The host applies each reliable button's position atomically before its transition; stale lossy pointer packets cannot override a later click barrier. A late reliable transition is retained even when newer motion has arrived.
- Invalid/oversized events are ignored. Reliable queue overflow ends the session, never silently discarding a key-up. Shutdown and native-pipeline failure release held keys/buttons.

## Trust boundary

HTTPS origin delivery is trusted, as with any browser cryptographic application. Transcript signatures prevent a relay-only adversary from substituting WebRTC identities, but cannot protect against compromised JavaScript delivered by that trusted HTTPS origin. Pairing links are bearer invitations; they are displayed only in the local host UI/explicit CLI output and are excluded from diagnostic logs.
