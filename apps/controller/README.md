# Lantern controller prototype

Browser half of the Stage A same-LAN remote-control spike. It requires a secure HTTPS origin in production because device identity uses WebCrypto and IndexedDB.

## Run locally

```sh
pnpm install
VITE_SIGNALING_URL=ws://127.0.0.1:8787/ws pnpm dev
```

If `VITE_SIGNALING_URL` is absent, the controller uses `/ws` on its current origin. Production therefore uses `wss://<public-origin>/ws` through the reverse proxy.

## Pairing link

The host opens a URL with this fragment:

```text
#host=<sha256-of-host-public-key>&key=<base64url-uncompressed-p256-public-key>&secret=<base64url-32-random-bytes>&name=<display-name>
```

The fragment is removed from the address bar after parsing. The secret is never included in a relay message. The controller stores a non-extractable P-256 identity key in IndexedDB and verifies the host identity before persisting a paired device.

The WebRTC connection uses no STUN or TURN servers. It gathers host and mDNS candidates for same-LAN use, receives one H.264 video track, and opens reliable `control` plus unordered/no-retransmit `pointer` data channels.

## Verify

```sh
pnpm test
pnpm build
pnpm smoke
```

The smoke test uses the locally installed Chrome channel. Safari and iPad playback/input remain physical-device gates.
