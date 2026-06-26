# MoyMoy (MochiOS Mobile 版) — DEV

MochiOS2.0 プラットフォーム向けの電子マネー / ウォレット / 送金アプリ。
完全な先行事例 **PiggleShop2** を最重要テンプレートとして踏襲する3点モノレポ。

実装計画の正本: `C:\Users\dora2\.claude\plans\mochios-mobile-moymoy-mochios-mobile-moy-memoized-puffin.md`

---

## プロジェクト仕様書

- **目的**: MochiOS Mobile 上で動く電子マネー/ウォレットアプリ。通貨単位は整数「エメ」、1エメラルド=1エメ。
- **構成（3コンポーネント）**:
  - `server/moymoy-cs/` — Rust+axum バックエンド → `moymoy.cs.mnn`。**トンネル内蔵型SDK**(`mochi-hub-cs-sdk` の `CsTunnel::start`)。ウォレットの唯一の権威。SQLite 永続化。**MC mod 無しでも完全動作**。
  - `app-mobile/apps/com.mochi.moymoy/` — HTML/JS バンドル。`fetch("https://moymoy.cs.mnn/...")`。デザイン「MochiOS Mobile.html」駆動。
  - `mod/` — Forge 1.20.1 MC サーバーサイドmod → `moymoy.mc.mnn`。エメラルド消費/付与の真実。connector の `CommandDispatch.Handler` を `register("moymoy", …)`。**オプショナル**。
- **エメラルドチャージ**: アプリ起点＋ゲーム内 両対応。双方向コマンドバス（backend が `cs_hosts:["moymoy"]` を claim、`reliable_send` 送信 / `run_inbound` 受信）。
- **整合性**: 「消費の真実=mod / 残高の権威=backend」を `emerald_ops` 台帳 + 二層冪等キー(`idem_key`/`op_id`) + at-least-once 再送 + 冪等決済で eventually-consistent に。
- **方針**: 旧 MoyMoy(`D:\IdeaProjects\MoyMoy`)はドメインの緩い参考のみ。MochiOS2.0 本体は原則無改変（app_backends 配置・`hosted_app_ids` 追加・証明書発行のみ）。

---

## 現在の仕様（デザイン「MochiOS Mobile.html」駆動で確定）

デザインは PayPay×クレジットカード風のエメラルド決済アプリ。タブは **home / send(送る) / pay(支払う) / charge(チャージ) / history(履歴)**。
通貨は整数「エメ」、**9エメ = 1エメラルドブロック**（Minecraft）。
取引種別 `kind`: `pay`(支払い) / `send`(送金) / `receive`(受取) / `charge`(チャージ)。各取引 `{id, kind, label, amount(符号付), ts}`。
**請求/承認(request/approve)機能はデザインに無い** → 実装しない。

UIフロー:
- **home**: 利用可能残高 + カード(holder/number/expiry) + クイックアクション(pay/send/charge) + 最近の取引4件。
- **send**: フレンド(プレイヤー)選択 → 金額 → 確認 → 完了。残高減・相手は receive。
- **pay**: 近くの加盟店選択 → 金額 → 確認 → 完了。残高減・加盟店口座へ。
- **charge**: インベントリ(手持ちエメラルド + ブロック、9エメ=1ブロック)を換算 → 金額 → 確認 → 完了。エメラルド消費し残高加算。**MC mod 依存**。
- **history**: 全取引リスト(フィルタ: すべて/支払い/送金/チャージ)。

### バックエンド HTTP API
- `GET /healthz`
- `GET /wallet/status` → `{ok, app:"moymoy", can_charge}`
- `GET /wallet/home?mc_uuid=&mcid=` → `{ok, balance, profile:{holder,number,expiry}, txns:[...recent], can_charge}`（home集約）
- `GET /wallet/history?mc_uuid=&limit=&filter=all|pay|send|charge` → `{ok, txns}`
- `GET /wallet/friends?mc_uuid=` → `{ok, friends}`（最近の相手 + コンタクト）
- `GET /wallet/merchants?mc_uuid=` → `{ok, merchants}`（登録加盟店。距離はMC presence依存）
- `GET /wallet/inventory?mc_uuid=` → `{ok, emeralds, blocks, chargeable}`（**MC mod 依存**。無ければ can_charge=false）
- `POST /wallet/send {idem_key, from_uuid, to_uuid|to_mcid, amount, memo?}` → P2P送金
- `POST /wallet/pay {idem_key, mc_uuid, merchant_id, amount, memo?}` → 加盟店支払い
- `POST /wallet/charge {idem_key, mc_uuid, amount}` → エメラルド→エメ（MC mod 消費、§チャージ整合）
- `GET /wallet/op?op_id=` → チャージ進捗（pending/sent/settled/failed）

### SQLite スキーマ
`accounts`(account_id=mc_uuid, mcid, balance, holder, card_number, card_expiry) / `transactions`(kind, label, counterparty, amount符号付, balance_after, ts) / `merchants`(merchant_id, account_id, name, …) / `idempotency` / `emerald_ops`。詳細は `server/moymoy-cs/src/db/schema.sql`。

### コマンドバス verb（backend→mod / mod→backend ack）
- `emerald.charge {op_id, idem_key, target_uuid, amount}` → mod 消費(インベントリのエメラルド+ブロック) → ack `{op_id, status, settled:consumed}`
- `inventory.query {req_id, target_uuid}` → mod が手持ちを返答 `{req_id, emeralds, blocks}`（charge画面のインベントリ表示用。任意）

---

## 問題 / 課題

- **段階0 ブロッカー**: デザイン「MochiOS Mobile.html」は `claude_design` MCP（要 `/design-login`）で取得。未取得の間はフロントエンドと design-derived エンドポイント/カラムが未確定。
- エメラルドチャージの致命ウィンドウ（consume成功・ack喪失・SavedDataフラッシュ前クラッシュ）は台帳+reconciliation+`setDirty()`直後フラッシュで最小化（exactly-once は原理的限界）。

---

## TODO

- [ ] 段階1: バックエンド基盤（Cargo.toml + 内蔵トンネル + TLS）
- [ ] 段階1: SQLite層 + ウォレットドメイン
- [ ] 段階1: HTTP API層（MC無しで動く最小ウォレット）
- [ ] 段階0: デザイン取込（要 /design-login）→ 仕様確定
- [ ] 段階2: フロントエンドバンドル
- [ ] 段階3: コマンドバス + チャージ整合
- [ ] 段階3: MC サーバーサイドmod
- [ ] 段階4: 配置・公開・E2E 検証
