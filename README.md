# Lantern

Rust 原生被控端與瀏覽器控制端的遠端桌面技術原型。Mac／Windows 擷取與編碼、區網 WebRTC、持久配對及控制輸入已實作；**完整第一版仍受階段 A 實機關卡限制，不能視為已驗收產品**。

- 被控端：macOS 14+、Windows 11 x64；已登入、未鎖定且保持喚醒的桌面。
- 控制端：桌面 Chrome、iPad Safari。控制裝置不需安裝 App。
- 連線：HTTPS／WSS 只交換配對和連線資訊；H.264 與控制指令經 WebRTC 直連。沒有 STUN、TURN 或雲端帳號。
- 金鑰：原生私鑰存入 OS credential store，瀏覽器不可匯出私鑰存入 IndexedDB。清除網站資料後需重新配對。

## 在開發電腦執行

需要 Rust 1.96+、Node 20.19+ 或 22+、pnpm 10.26，以及對應原生平台工具鏈。Windows 需要 Visual Studio C++ build tools；Mac 需要 Xcode command line tools。

先建置網站：

```sh
cd apps/controller
pnpm install --frozen-lockfile
pnpm build
cd ../..
```

終端機一啟動服務。開發模式明確使用 localhost；不提供不安全的 LAN HTTP 遠控入口：

```sh
cargo run -p lantern-signaling
```

終端機二啟動被控端。首次執行會在 OS credential store 建立這個 App 自己的身分金鑰：

```sh
cargo run -p lantern-host -- --server http://localhost:8787 --allow-insecure-localhost --name "My Desktop" --pair
```

以 Chrome 開啟終端機顯示的配對連結，或掃描配對 QR Code。**localhost 連結僅供同一台電腦測試；iPad 與其他電腦必須使用下面的 HTTPS 部署。**此原型不會在啟動時擷取螢幕；成功驗證已配對瀏覽器的連線請求後才開啟原生管線。

Mac 首次實際控制前，需在「系統設定 → 隱私權與安全性」授予執行程式「螢幕錄製」與「輔助使用」權限，再重新啟動。CLI 的權限可能歸屬啟動它的終端機；日常測試應優先使用桌面診斷 App。Windows 使用一般使用者桌面，不控制 UAC、安全桌面或較高權限視窗。

以 `Ctrl+C` 停止 CLI。重新執行同一資料目錄的主機後，網站可用已保存裝置再次連線。CLI 管理配對：

```sh
cargo run -p lantern-host -- --list-pairs
cargo run -p lantern-host -- --revoke <browser-id>
```

如使用 `--data-dir`，管理命令也需指定相同目錄。撤銷會由執行中的主機在下一次狀態檢查斷開該瀏覽器。

前端熱更新另開 `pnpm dev`，並使用：

```sh
cargo run -p lantern-signaling -- --origin http://localhost:5173
cargo run -p lantern-host -- --server http://localhost:5173 --allow-insecure-localhost --pair
```

Vite 將 `/ws` 代理至 8787。兩種啟動方式擇一；瀏覽器 Origin 必須與服務設定完全一致。

## HTTPS 區網測試

準備指向部署機器的網域，並開放 80／443 給 Caddy 取得 TLS 憑證：

```sh
cp .env.example .env
# 修改 LANTERN_DOMAIN 與 ACME_EMAIL
docker compose up --build -d
```

在 Mac／Windows 執行：

```sh
cargo run -p lantern-host -- --server https://remote.example.com --name "My Desktop" --pair
```

控制裝置與被控端仍需在相同 LAN。服務可位於公網；被控端主動建立 WSS，不需向網際網路開放被控端 TCP port。瀏覽器可能詢問本機網路權限。防火牆或 AP 隔離阻擋直連時，原型會失敗，不會改走中繼。

## 驗證

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check -p lantern-platform --tests --target x86_64-pc-windows-msvc
cd apps/controller
pnpm test
pnpm build
pnpm smoke
```

`pnpm smoke` 預設使用已安裝 Chrome。CI 可設定 `PLAYWRIGHT_CHROMIUM=bundled`，先執行 `pnpm exec playwright install chromium`。

Rust 有真實 loopback ICE／DTLS／SRTP／SCTP 測試，使用明確標示的黑色影片 fixture 與記錄用輸入；不擷取使用者畫面，也不向系統注入輸入。此測試不能取代實機遠控驗收。

Apple Silicon 內測包已產生於 `releases/`，可執行 `./scripts/package-macos.sh` 重建並檢查簽章／雜湊。Windows 安裝包尚待 Windows 建置與實機驗收。

桌面診斷 App 的建置與操作方式見 [apps/desktop/README.md](apps/desktop/README.md)。架構及驗收紀錄見 [docs/STATUS.md](docs/STATUS.md) 與 [docs/PROTOCOL.md](docs/PROTOCOL.md)。

## Git

此目錄是獨立 Git repository，預設分支 `main`。依可驗證的實作階段提交。未設定 remote，也未推送到外部服務。

## Container registry

GitHub Actions 的 **Publish container** workflow 可在 `main` 手動執行：先建置並測試容器的 `/healthz`、首頁與網站資產，再推送 `linux/amd64`／`linux/arm64` 到 `ghcr.io/hsbearbig/lantern`。

它使用專案的 `GITHUB_TOKEN`，不需要另存個人 registry 密碼。每次成功發布會產生 `latest` 與 `sha-<完整 commit SHA>` 標籤，並在 workflow summary 記錄 image digest。這個 workflow 不會因一般 push 自動發布。

```sh
docker pull ghcr.io/hsbearbig/lantern:latest
```

Package 的存取權由 GitHub Container Registry 設定決定；private package 拉取時需要 registry 登入。此 image 包含瀏覽器控制端與連線協調服務，被控端 App 另外安裝。

套件腳本只允許 esbuild，使用 pnpm 10.26+ 的 `allowBuilds` 設定。[pnpm 官方設定說明](https://pnpm.io/10.x/settings#allowbuilds)
