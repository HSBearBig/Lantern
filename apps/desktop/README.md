# Lantern Host diagnostic app

Minimal Tauri 2 shell for the Stage A native-host spike. It starts and supervises `lantern-host`, displays the actor's real status, generates the actor's one-time pairing QR, and exposes only the controls needed for physical Chrome/iPad validation.

The app does not start remote access on first launch. Enabling “登入後啟動” registers the local autostart plugin and persists that preference; later launches start the host actor with the saved HTTPS URL. Screen capture itself begins only when an authenticated browser creates a remote session.

## Development

```sh
pnpm install --frozen-lockfile
pnpm app:dev
```

Use an HTTPS controller/signaling origin. For local development only, enter `http://localhost:<port>` and explicitly enable the localhost HTTP checkbox for that run. The opt-in is deliberately not persisted.

## Verification

```sh
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
pnpm tauri build --no-bundle
```

The final command creates a native executable without opening it, requesting capture permissions, or starting a host session.

For an Apple Silicon/Intel package on the corresponding Mac, run `./scripts/package-macos.sh` from the repository root. It builds the App, applies and verifies a local ad-hoc signature, and creates a DMG/ZIP plus SHA-256 checksums in `releases/`. It uses `hdiutil` without Finder automation. These are internal test packages, not Developer ID-signed or notarized public releases. The locally produced package is Apple Silicon only; Intel and Windows packages require their corresponding build/test lanes.

The CI workflow includes a Windows NSIS installer lane, but it has not been executed in this local project. No Windows installer is included in the local deliverables.

## Local data

- `settings.json` stores only the controller URL, device name, and autostart preference.
- Host identity remains in the operating-system credential store through `lantern-host`.
- Paired browser public keys remain in the host data directory.
- Pairing secrets are shown only in the requested QR/copy-link flow and are not logged.
