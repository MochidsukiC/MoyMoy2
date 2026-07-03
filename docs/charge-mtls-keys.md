# MC コマンドバス mTLS 鍵リファレンス（エメラルドチャージ）

MoyMoy のエメラルドチャージは Hub の MC コマンドバス（QUIC :7421, mTLS）経由で
動く。鯖缶（サーバ運用者）が実際に発行・配置しなければならない鍵と、その出所を
一覧化する。

## 1. 要約（鯖缶が握るべき鍵は結局これだけ）

- **CA は Hub が1つだけ自動管理**する（運用者は明示的に発行操作しない限り触らない）。
- 運用者が発行・配置するのは次の **2種類だけ**:
  1. 各 MC サーバの **コネクタ leaf**（`mcserver-id` 毎、mod がサイドカーに渡す）
  2. 各 CS backend の **leaf**（アプリ毎、例: moymoy）
- **Hub サーバ証明書（:7421 側）は毎起動 CA から自動派生**し、ファイル管理不要（ノータッチ）。

## 2. 全体図

```
CS backend (moymoy-cs)                              MC コネクタ サイドカー
   client leaf (id=moymoy)                            client leaf (id=mc1 等)
        |                                                     |
        |  mTLS (chain.pem+leaf.key.pem, ca.cert.pem で検証)   |
        v                                                     v
        +-------------------> Hub :7421 (QUIC) <--------------+
                    server 証明書は CA から毎起動 in-memory 派生
                    (ディスク管理不要、DNS SAN既定 localhost)

  すべて同一の CA（Hub の mc-pki）が発行した鍵でなければ mTLS 検証に失敗する
  （BadSignature）。
```

## 3. 鍵一覧テーブル

| 主体 | 種別 | 識別子 | 発行モジュール | 発行コマンド/関数 | 出力ディレクトリ(発行時) | 最終配置先 | 参照方法(env等) | 含むファイル |
|---|---|---|---|---|---|---|---|---|
| Hub CA（信頼の根） | CA | — | crate `mochi-hub-mc-pki`（`McPki::load_or_generate`） | Hub 起動時に自動 or `mochi-mc-ca init --dir <dir> --flat` | — | `<state_dir>/mc-pki`（`MOCHI_HUB_MC_PKI_DIR` 未設定時のフォールバック。`hub/server/src/main.rs`） | Hub 環境変数 `MOCHI_HUB_MC_PKI_DIR`（未設定なら `<state_dir>/mc-pki`） | `root.cert.pem` / `root.key.pem`（flat モード） |
| Hub サーバ証明書(:7421) | サーバ | DNS SAN（既定 `localhost`） | crate `mochi-hub-mc-pki`（`McPki::issue_server_cert_der`, `McBusController::start` 内） | 起動時に自動（CLI で手動発行するなら `mochi-mc-ca server-cert --dir <CA> --dns localhost --out <dir> --flat`） | — | ディスクに書かない（in-memory、毎起動 CA から派生） | — | — |
| MC コネクタ leaf（サイドカー） | クライアント | `mcserver-id`（例: `mc1`） | CLI バイナリ `mochi-mc-ca`（crate `mochi-hub-mc-pki`） | `mochi-mc-ca issue --dir <CA> --mcserver-id mc1 --out <dir> --flat`（または Hub TUI の `issue_profile`） | 任意（例 `.devstack/mc-pki/mc1`） | MC サーバ側 `<gameDir>/config/mochi/key`（mod の `connector.cert_dir` 既定値。実測ログでは `forge/run/config/mochi/key`） | サイドカー `mochi-mc-connector` の環境変数 `MOCHI_MC_CERT_DIR`（mod が spawn 時に設定） | `chain.pem` / `leaf.key.pem` / `ca.cert.pem` |
| backend leaf（moymoy-cs） | クライアント | `moymoy` | CLI バイナリ `mochi-mc-ca`（crate `mochi-hub-mc-pki`） | `mochi-mc-ca issue --dir <CA> --mcserver-id moymoy --out <dest>/mc-cert --flat`（`deploy-backend.ps1 -EnableCharge` が実行） | `<HubWorkdir>/app_backends/moymoy/mc-cert` | 同上（backend の workdir 相対） | moymoy-cs の環境変数 `MOCHI_MC_CERT_DIR`（`app.toml` に `"mc-cert"`）、Hub アドレスは `MOCHI_MC_HUB_QUIC`（既定 `127.0.0.1:7421`） | `chain.pem` / `leaf.key.pem` / `ca.cert.pem` |

補足:
- CA・コネクタ leaf・backend leaf はいずれも `mochi-mc-ca issue`/`init` が出す 4 ファイル名のうち、`Issue` サブコマンドは `leaf.cert.pem` / `leaf.key.pem` / `chain.pem` / `ca.cert.pem` を書く（`mochi-mc-ca.rs`）。一方 backend/コネクタが実際に読むのは `chain.pem` / `leaf.key.pem` / `ca.cert.pem` の3つ（`leaf.cert.pem` は使わない）。
- Hub TUI 経由の `issue_profile`（`mc_bus.rs`）は `chain.pem` / `leaf.key.pem` / `ca.cert.pem` の3ファイルのみを書く（`leaf.cert.pem` は書かない）— CLI と TUI 経由で出力ファイル集合がわずかに異なる（実装上の相違、要注記）。

## 4. 発行手順（コピペ可能）

**重要: すべて同一 CA（Hub の `mc-pki` ディレクトリ）を `--dir` に使うこと。**

### (a) MC コネクタ leaf（開発時: `mc-connector-dev.ps1` 経由）

```powershell
powershell -File tools/mc-connector-dev.ps1 -McserverId mc1
# 内部で: mochi-mc-ca init --dir .devstack\mc-pki\ca --flat
#         mochi-mc-ca issue --dir .devstack\mc-pki\ca --mcserver-id mc1 --out .devstack\mc-pki\mc1 --flat
```

直接コマンドで発行する場合（本番 CA を使う場合は `--dir` を Hub の実 CA に変更）:

```powershell
mochi-mc-ca issue --dir <Hub の mc-pki CA> --mcserver-id mc1 --out <forge>/run/config/mochi/key --flat
```

### (b) backend（moymoy-cs）leaf

```powershell
powershell -File tools/deploy-backend.ps1 -HubWorkdir <HubWorkdir> -EnableCharge
```

`-McCaDir` の既定は **`<HubWorkdir>\state\mc-pki`**（＝Hub の正準 CA。`mc_bus.rs`
が `MOCHI_HUB_MC_PKI_DIR` 未設定時に `<state_dir>/mc-pki` にフォールバックする先）
なので、Hub の CA がここにある通常構成では **`-McCaDir` の指定は不要**。Hub 側で
`MOCHI_HUB_MC_PKI_DIR` を別ディレクトリに設定している場合だけ、その値を `-McCaDir`
で渡す。

直接コマンド:

```powershell
mochi-mc-ca issue --dir <HubWorkdir>\state\mc-pki --mcserver-id moymoy --out <HubWorkdir>/app_backends/moymoy/mc-cert --flat
```

## 5. よくある落とし穴

- **CA 不一致 = `BadSignature`**（今回の不具合の原因）。leaf は必ず Hub が :7421 に使う CA と同一 CA（`<HubWorkdir>/state/mc-pki`）から発行する。`deploy-backend.ps1 -EnableCharge` は既定でこの CA を使うが、Hub 側で `MOCHI_HUB_MC_PKI_DIR` を別ディレクトリに向けている場合は `-McCaDir` でそれに合わせる。旧 `.devstack/mc-pki/ca` 由来の leaf は今の Hub CA と食い違うので使わない。
- コネクタ leaf の **`--mcserver-id` と配置先ディレクトリの対応**を取り違えない（mod は `connector.cert_dir` に置かれたものを無条件でその ID の leaf として読む）。
- 再発行後は **該当プロセス（backend または mc-connector サイドカー）の再起動が必要**（起動時に一度だけ証明書を読み込むため、動的リロードはしない）。
- backend（moymoy-cs）の証明書読み込みは `CommandBus::connect` 内で起動時 1 回のみ。`MOCHI_MC_CERT_DIR` 未設定または3ファイル欠落時はサイレントに「ウォレットのみモード」（`can_charge=false`）に落ちる（エラーにはならない）。

## 6. チャージ以外の鍵との区別（混同注意）

ウォレット経路（cs.mnn 通信、Hub :7411/:7420）は **mTLS 証明書ではなく bearer トークン**
（`MOCHI_TUNNEL_BEARER`）で認証する、この文書で扱う MC コマンドバス mTLS とは
**別系統**。両者を混同して同じ鍵/トークンを使い回さないこと。

---

今回の対応: backend leaf を `run/state/mc-pki`（Hub の実 CA）から再発行済み。backend 再起動で反映。
