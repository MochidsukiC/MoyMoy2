# MoyMoy デプロイ手順

3コンポーネント（バックエンド `moymoy.cs.mnn` / モバイルアプリ / MCサーバーサイドmod `moymoy.mc.mnn`）の配置と検証。MochiOS2.0 本体は無改変（app_backends 配置・`hosted_app_ids` 追記・証明書発行のみ）。

## 前提: devstack 起動
```
# MochiOS2.0 で hub + app-registry(:7405) + app-repository(:7409)
#                + accounts(:7404) + auth(:7402) + gateway(:7411)
#                + router(:7400) + QUIC(:7420/:7421) を起動
D:\IdeaProjects\MochiOS2.0\tools\mochi-inworld.ps1
```

## 1. バックエンド（moymoy.cs.mnn）
```
# ビルド + Hub workdir の app_backends/moymoy/ へ配置（app.toml + バイナリ）
tools\deploy-backend.ps1 -HubWorkdir <Hubの作業ディレクトリ>

# app_backends/moymoy/app.toml を編集し MOCHI_TUNNEL_BEARER を設定
#   （チャージも使うなら MOCHI_MC_CERT_DIR も。下記 3 参照）
# enabled = true なので launcher が起動。Hub TUI で状態確認。
```
内蔵トンネル（`tunnel = "self"`）で `moymoy.cs.mnn` を自己 claim。MC証明書未設定なら `can_charge=false` のウォレットのみで動作。

## 2. モバイルアプリ
```
# セッショントークン取得（OTPオフなら open registration で自作可）:
$body = @{ email='you@example.com'; password='pw' } | ConvertTo-Json
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7404/accounts -ContentType application/json -Body $body
$tok = (Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7402/auth/login `
          -ContentType application/json `
          -Body (@{email='you@example.com';password='pw';device_id='dev'}|ConvertTo-Json)).access_token

# 公開（pack → registry POST → repository PUT）
tools\publish-moymoy.ps1 -Token $tok
```
in-world の App Store（com.mochi.appstore）から「MoyMoy」をインストール → ホームに表示 → 起動。

## 3. MCサーバーサイドmod（moymoy.mc.mnn、オプショナル）
```
# MochiOS2.0 の forge を先にビルド（compileOnly 参照先）
# mod をビルド
cd mod ; .\gradlew build       # → build/libs/moymoy-0.1.0.jar

# jar を mochi connector mod と一緒に MCサーバの mods/ へ
# mochi-server.toml [connector].hosted_app_ids に "moymoy" を追加

# バックエンド用 MC クライアント証明書を発行し app.toml の MOCHI_MC_CERT_DIR に設定
#   （mochi-mc-ca の場所は MochiOS2.0/hub/mc-pki/src/bin/mochi-mc-ca.rs）
cargo run -p mochi-hub-mc-pki --bin mochi-mc-ca -- issue --mcserver-id moymoy --out <cert_dir>
```

## ローカル開発（Hub/MC 不要、ブラウザ検証）
```
# 端末1: バックエンド（TLS/トンネル off、dev-credit on）
tools\run-cs.ps1
# 端末2: バンドル静的サーバー
tools\dev-serve.ps1
# ブラウザ（?mcid= は charge 用の MC キャラ。MoyMoy 口座は UI の「口座開設」で handle+PIN 作成）:
#   http://127.0.0.1:8099/dev.html?moymoy_http=http://127.0.0.1:7433&mcid=Steve
# 残高投入（UI で @alice を開設後、dev-credit は handle 指定）:
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7433/wallet/_dev/credit `
  -ContentType application/json -Body (@{handle='alice';amount=12480}|ConvertTo-Json)
```

## E2E 検証
- **A. ウォレット単体（MC不要）**: 口座開設(handle+PIN)→ `/wallet/status` `can_charge:false`。dev-credit(handle) で残高投入 → ホーム反映。`@相手` へ送金（相手に receive）・加盟店支払い・履歴。同一 idem_key で二重送金されない。**1端末に2口座を開設→切替で残高が独立**。リロードで保存セッション自動ログイン。PIN 連続失敗でロックアウト。backend 再起動で SQLite から復元。
- **B. チャージ（mod + 証明書あり）**: `/wallet/status` → `can_charge:true`。プレイヤー在線中にアプリの「チャージ」→ 在世エメラルド消費 → 残高加算。`/eme` でローカルの換金可能量表示。
- **C. 整合・冪等**: ack 一時喪失 → `emerald_ops` が sent 滞留 → reconcile 再送 → mod が duplicate 再ack（再消費なし）→ settled。MCサーバ再起動を跨いで二重消費なし（EmeraldOpStore）。
