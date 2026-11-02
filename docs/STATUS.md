# Implementation and acceptance status

This is a Stage A diagnostic implementation, with the minimum authentication needed to test safely. The approved full V1 plan has **not** passed its hardware gate. Full product UI work and claims of supported performance remain deferred.

## Implemented

| Area | Implementation | Evidence boundary |
|---|---|---|
| macOS | Persistent SCStream, latest-frame mailbox, VideoToolbox H.264, CGEvent/Unicode, permission preflight | Compiles; conversion/mailbox/key-mapping tests. No screen capture/input runtime acceptance yet |
| Windows | Windows.Graphics.Capture/D3D11, hardware/software Media Foundation H.264, SendInput, COM/WinRT lifecycle | Windows-target code and tests cross-compile; no Windows execution on the Mac development host |
| Native transport | webrtc-rs H.264 and reliable/lossy DataChannels, bounded queues, keyframe feedback, input cleanup | Real loopback UDP ICE/DTLS/SRTP/SCTP fixture/control test |
| Authentication | OS-stored host identity, browser WebCrypto identity, single-use invitation, signed SDP/challenge, revoke | Unit/socket security tests, shared Rust/WebCrypto vectors, pair-persist-revoke lifecycle; no key material in logs |
| Browser | Persistent device list, IME text submission, pointer/keyboard/touch, reconnect/timeout diagnostics | 17 unit tests; typecheck/build; 3 real Chrome checks including IndexedDB reload and real relay/proxy |
| Signaling/deployment | Bounded in-memory WSS relay, exact Origin, HTTPS Caddy/Docker recipe | Socket tests and local HTTP smoke; external HTTPS deployment not performed |
| Desktop shell | Minimal Tauri diagnostic host shell | Apple Silicon App/DMG/ZIP built; ad-hoc signature verified; 2 settings tests. App not launched |
| Git | Independent repository on main | Local commits; no external remote |

## Required physical matrix — still pending

| Host | Controller | Capture/decode/input | 30-minute stability | p95 <=250 ms |
|---|---|---|---|---|
| Physical Apple Silicon Mac | Desktop Chrome | Pending | Pending | Pending |
| Physical Apple Silicon Mac | Physical iPad Safari | Pending | Pending | Pending |
| Physical Windows 11 x64 | Desktop Chrome | Pending | Pending | Pending |
| Physical Windows 11 x64 | Physical iPad Safari | Pending | Pending | Pending |
| Physical Intel Mac | Supported browsers | Pending | Pending | Pending |

A passing simulated, loopback or cross-compile test does not mark a physical row complete. No user screen was captured and no real system input was injected during automated validation.

## Manual acceptance procedure

1. Deploy the controller and relay to one trusted HTTPS origin. Use one host and controller on the same normal LAN without AP isolation. Record OS/browser/GPU/display/DPI/network conditions.
2. Start the App, grant platform permissions, enable the host, pair once, and verify the host-key identity stays unchanged after App/browser restarts.
3. Verify click/drag/scroll/letterboxing, modifier shortcuts, Chinese and emoji submission. On iPad test tap/right-click/two-finger scroll/drag mode and background/foreground transitions.
4. Attempt a second controller while connected; expect busy. Revoke the active browser; expect disconnect and released keys. Re-pairing requires a fresh invitation.
5. Revoke Screen Recording/Accessibility, lock the desktop, sleep/wake, change network and stop the relay. Failures must be visible; no stale held input. An established P2P session must survive a relay-only outage.
6. Run 30 minutes at up to 1080p30 on hardware encoding. Record memory and queues, capture/encode rate, actual browser decode FPS and dropped frames.
7. Measure screen-to-screen and input-to-visible-response latency with an external high-frame-rate recording; calculate p95 from repeated samples. WebRTC RTT alone is not that measurement.
8. Limit bandwidth, verify no growing backlog, force loss/keyframe recovery, then restore network. Quality adaptation and recovery require physical proof before acceptance.

## Remaining work after the gate

- Tune hardware encoder compatibility, software 720p fallback thresholds and feedback-based quality adaptation from the measured matrix. Current prototype has explicit software/height options and hardware fallback; automatic sustained-load downshift/upshift is not complete.
- Finalize product permission guidance, full UI/UX and installation experience after native paths pass.
- Complete physical lock/sleep/permission-revocation behavior; these states are outside the controllable desktop support boundary and must fail visibly.
- Build Windows installers on Windows, Intel/universal Mac distribution on the appropriate lane, and actual iPad/current-previous-browser tests. Local Mac checks do not certify these deliverables.
- Credentials for public signing/notarization and actual hosted deployment are not part of the internal prototype's automated validation.

## Local validation report — 2026-11-02 (Asia/Taipei)

| Check | Result |
|---|---|
| `cargo test --workspace` | 25 checks passed (includes 4 generated-type export checks); real loopback video/control test included |
| Strict workspace Clippy and rustfmt | Passed |
| Windows platform `cargo check --tests --target x86_64-pc-windows-msvc` | Passed; compile only |
| Desktop `cargo test --lib` | 2 passed; does not launch an App or host |
| Desktop strict Clippy and frontend production build | Passed |
| Controller Vitest | 17 passed |
| Controller Chrome Playwright | 3 passed: diagnostics, persistent nonextractable key, real relay/proxy plus invitation/StrictMode regression |
| Mac App packaging | Apple Silicon App/DMG/ZIP produced; ad-hoc signature and Info.plist verified |
| Docker Compose configuration | Validated |
| Docker image/container execution | Not run: local Docker daemon is unavailable |
| Physical device discovery | iPad discovery could not initialize CoreDeviceService; no iPad runtime validation |
| Public HTTPS deployment / GitHub CI / Windows installer | Configured as source, not executed locally |

The first independent integration review found cross-channel click ordering, unbounded connection establishment and mismatched development URLs. These were fixed with atomic button coordinates/sequence barriers, timeout/stale-session tests and an actual browser-to-relay proxy regression. A subsequent independent review did not complete because the subagent workspace reported exhausted credits; no final independent approval is claimed.
