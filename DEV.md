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

デザインは 電子マネー×クレジットカード風のエメラルド決済アプリ。タブは **home / send(送る) / pay(支払う) / charge(チャージ) / history(履歴)**。
通貨は整数「エメ」、**9エメ = 1エメラルドブロック**（Minecraft）。
取引種別 `kind`: `pay`(支払い) / `send`(送金) / `receive`(受取) / `charge`(チャージ)。各取引 `{id, kind, label, amount(符号付), ts}`。
**請求/承認(request/approve)機能はデザインに無い** → 実装しない。

UIフロー:
- **home**: 利用可能残高 + カード(holder/number/expiry) + クイックアクション(pay/send/charge) + 最近の取引4件。
- **send**: フレンド(プレイヤー)選択 → 金額 → 確認 → 完了。残高減・相手は receive。
- **pay**: 近くの加盟店選択 → 金額 → 確認 → 完了。残高減・加盟店口座へ。
- **charge**: インベントリ(手持ちエメラルド + ブロック、9エメ=1ブロック)を換算 → 金額 → 確認 → 完了。エメラルド消費し残高加算。**MC mod 依存**。
- **history**: 全取引リスト(フィルタ: すべて/支払い/送金/チャージ)。

### アカウントモデル（v2・独立アカウント + PIN）
**独立した MoyMoy アカウント（電子マネー型）**。`account_id` はサーバ生成 UUID で、Minecraft UUID とは独立。

- **資格情報**: `handle`（一意・小文字正規化・`[A-Za-z0-9_]` 3〜20）＋ `PIN`（4〜6桁数字, **Argon2id** ハッシュ保存）。handle は送金宛先（`@handle`）に兼用。
- **セッション**: register/login で 256bit ランダムトークンを発行し、HTTP ヘッダ `X-MoyMoy-Session` で送る。DB には **SHA-256 ハッシュ**で保存（期限 30日・logout で失効）。**backend が全ウォレットリクエストの本人を検証**（旧 mc_uuid 自己申告を解消）。
- **マルチアカウント**: 1端末に複数口座をリンク。クライアント保持リスト（`mochi.storage` / dev は localStorage）が正本で、ヘッダのアバターから切替・追加・ログアウト。サーバは `moymoy_sessions.phone_id` をメタデータ記録のみ。
- **MCキャラ連携**: チャージ時に現在の `gameUuid`（os.gameUuid）を `account_mc_links` へ自動リンク（1口座に複数キャラ可）。MC UUID は「チャージ用の連携リソース」。1キャラ=1口座（`account_mc_links.mc_uuid` UNIQUE、別口座からのチャージ/在庫照会は `character_claimed` で拒否）。
- **メール検証 / 2FA / リカバリ（v4）**: **MNN メール（`@*.mnn`）限定**。`MOCHI_MAIL_SERVICE_BEARER` 設定時は**開設にメール＋OTP必須**（1メール1口座、`email_lower` UNIQUE）、ログインは PIN＋メール2FA、PIN 忘れはメール OTP で再設定。未設定なら**従来の handle+PIN へ自動 degrade**。OTP は 6桁・SHA-256(+`MOYMOY_OTP_PEPPER`)保存・10分・5回上限・単回・再送クールダウン（`moymoy_otps`）。送信は `mochi-hub-mailer` の `MnnMailSender`（IPvM ゲートウェイ `/mail/otp-deliver` 経由で相手の in-world メールアプリへ配送。外部SMTPは使わない）。`valid_email` は `local@<単一ラベル>.mnn` のみ受理。dev 検証は `MOYMOY_DEV_OTP_LOG=1`（コードをログ出力）。

### バックエンド HTTP API
全レスポンス `{ok:bool, ...}`。ウォレット系は `X-MoyMoy-Session` でセッション認証（無効は 401）。
- `GET /healthz` / `GET /wallet/status` → `{ok, app:"moymoy", can_charge}`（公開）／ `GET /auth/config` → `{ok, email_enabled}`
- `POST /auth/register {handle, display_name, pin, email?, phone_id?}` → メール有効時 `{ok, pending:"verify_email", email}`／無効時 `{ok, session, account}` ／ `POST /auth/register/verify {email, code}` → `{ok, session, account}`
- `POST /auth/login {handle, pin, phone_id?}` → 2FA 時 `{ok, pending:"2fa", email}`／それ以外 `{ok, session, account}` ／ `POST /auth/login/verify {handle, code}` → `{ok, session, account}`
- `POST /auth/recover/start {handle}` → 常に `{ok}`（列挙防止） ／ `POST /auth/recover/verify {handle, code, new_pin}` → `{ok, session, account}`
- `POST /auth/logout`（session） ／ `GET /auth/me` → `{ok, account, email, email_verified, linked_mc:[{mc_uuid,mcid}]}` ／ `GET /auth/lookup?handle=` → 送金宛先解決
- `GET /wallet/home` → `{ok, balance, profile:{holder,number,expiry}, txns:[...recent], can_charge}`
- `GET /wallet/history?limit=&filter=all|pay|send|charge` ／ `GET /wallet/friends`（最近の相手・handle 付）／ `GET /wallet/merchants`
- `GET /wallet/inventory?mc_uuid=&mcid=` → `{ok, emeralds, blocks, chargeable}`（**MC mod 依存**）
- `POST /wallet/send {idem_key, to_handle, amount}` → `@handle` 宛 P2P 送金
- `POST /wallet/pay {idem_key, merchant_id, amount}` → 加盟店支払い
- `POST /wallet/charge {idem_key, amount, mc_uuid, mcid?}` → エメラルド→エメ（着金=account_id / 消費ルーティング=mc_uuid、自動リンク、§チャージ整合）
- `GET /wallet/op?op_id=` → チャージ進捗（所有権検証付き）
- `POST /wallet/_dev/credit {handle, amount}` → dev 専用クレジット（`MOYMOY_DEV_CREDIT=1` ゲート）

### SQLite スキーマ
`accounts`(account_id=MoyMoy口座UUID, handle, handle_lower, display_name, pin_hash, balance, holder, card_number, card_expiry, is_merchant, failed_pin_attempts, locked_until, **email, email_lower(UNIQUE), email_verified**) / `moymoy_sessions`(session_id, account_id, token_hash, phone_id, expires) / `account_mc_links`(account_id, mc_uuid(UNIQUE), mcid) / `moymoy_otps`(otp_id, purpose=signup|login2fa|recovery, email_lower, account_id, code_hash, payload_json, attempts, expires) / `transactions` / `merchants` / `idempotency`(PK=idem_key,scope) / `emerald_ops`(op_id, account_id=着金先, mc_uuid=消費キャラ, state=pending|sent|settled|failed|stuck, …)。マイグレーションは user_version ステッパ（v1 baseline `schema.sql` → v2 独立アカウント → v3 1キャラ1口座 → v4 メール/OTP、各 `db/schema_vN.sql`）。詳細は `server/moymoy-cs/src/db/`。

### コマンドバス verb（backend→mod / mod→backend ack）
- `emerald.charge {op_id, idem_key, target_uuid, amount}` → mod 消費(インベントリのエメラルド+ブロック) → ack `{op_id, status, settled:consumed}`
- `inventory.query {req_id, target_uuid}` → mod が手持ちを返答 `{req_id, emeralds, blocks}`（charge画面のインベントリ表示用。任意）

---

## 問題 / 課題

- **本人検証の到達点**: v2 で「自己申告 mc_uuid」→「backend が検証する handle+PIN セッション」へ移行し、ウォレットの本人性は MoyMoy 内で完結して検証可能になった。MochiOS のゲートウェイは cs.mnn 宛に検証済みアカウントを注入しない（調査確認済）ため、OS アカウント連携ではなく **MoyMoy 独自資格情報**で本人を担保している。
- **セッショントークンの保存**: クライアントは `mochi.storage`（in-world は per-app 隔離・再起動跨ぎ永続）/ dev は localStorage にトークンを保持。盗用時の被害は当該口座に限定され、logout・期限切れで失効。より強固にするなら端末バインドや短命トークン+リフレッシュを将来検討。
- **memo 未実装**: デザインの送金/支払いフローに memo 入力欄が無いため、API からも除外（受理して捨てる挙動は不採用）。必要時は transactions.memo への配線を追加。
- **in-game チャットコマンドからの backend 報告は不可**: `mochi` connector(`MochiMod`)は `DISPATCH`(inbound ルーティング)のみ公開で、ハンドラ外からの unsolicited 送信API が無い。よってエメラルドチャージは**アプリ起点**で完結する。真の `/eme deposit` には mochi connector への outbound 送信API追加（承認の要る MochiOS2.0 改変）が必要。
- エメラルドチャージの致命ウィンドウ（consume成功・ack喪失・SavedDataフラッシュ前クラッシュ）は台帳+reconciliation+`setDirty()`直後フラッシュで最小化（exactly-once は原理的限界）。
- **CodeX 再レビュー（反映済）**: v2 再設計に recursive-codex-reviewer を実施。妥当指摘を反映 — backend `382acc2`（冪等の複合PK化で二重決済防止 / `user_version` を tx 内へ移しマイグレーション原子化 / 握り潰しログ化 ほか）、frontend `ffb40c8`（`me()` を ok/expired/unknown で識別し一時エラーで口座を消さない / アンマウントガード / 401 即時処理 ほか）。
- **承認ゲート保留（共有層に跨る設計課題・未着手）**: 着手前に設計案の承認が必要。
  - **R007**: `/wallet/inventory` の mc_uuid 所有権検証。リンクは charge 時に確立されるため、未リンク照会を許容しつつリンク後のみ検証する設計が要る。
  - **R008**: `reconcile` の op TTL / dead-letter。`sent` の消費済みエメラルドを安全に失効させる escalate フロー（単純 TTL は消費済み無クレジット化の危険）。
  - **R05/R06**: SDK の `_session` がグローバルのため、非アクティブ口座の logout / 切替検証中に並行 API が誤セッションを送る競合。`getJson/postJson` への per-call トークン引数化で根治。
  - **R13 / charge 再試行**: `store.set` 失敗の握り潰し、チャージ poll タイムアウト後の再試行で別 op_id の二重消費窓。

---

## TODO

- [x] 段階0: デザイン取込（claude_design MCP）→ 仕様確定
- [x] 段階1: バックエンド基盤（Cargo.toml + 内蔵トンネル + TLS）
- [x] 段階1: SQLite層 + ウォレットドメイン
- [x] 段階1: HTTP API層（MC無しで動く最小ウォレット）— E2E検証済
- [x] レビュー指摘修正: 冪等の単一トランザクション化(TOCTOU二重支払い根絶)
- [x] 段階2: フロントエンドバンドル（デザイン駆動）
- [x] 段階3: コマンドバス + チャージ整合（emerald_ops 台帳）
- [x] 段階3: MC サーバーサイドmod（Forge、moymoy-0.1.0.jar ビルド済）
- [x] 段階4: 配置・公開ツール（tools/, deploy/, icon.png）
- [x] **再設計**: 独立アカウント(handle+PIN)+セッション検証 — backend（`6b85dc5`、HTTPスモーク緑）
- [x] **再設計**: マルチアカウント(1端末=複数口座)+1口座=複数MCキャラ — frontend（`9cc6c18`、Babel透過）
- [x] CodeX 再レビュー反映（v2: backend `382acc2` / frontend `ffb40c8`）
- [x] 承認ゲート課題の実装（R007 1キャラ1口座 / R008 dead-letter `1b95b62`、R05/R06/R13/charge再試行 `21d98cc`）
- [x] 再公開: バンドル v0.2.0 を GitHub リリース化＋registry再登録
- [x] **メール認証**: 検証/2FA/PINリカバリ/1メール1口座＋SMTP無しdegrade — backend `01a4b0f`(schema v4, スモーク緑) / frontend `cf702d6`(Babel透過)
- [x] CodeX 再レビュー反映（R007/R008・frontend-followups `cc15389` ＋ メール認証 `9b910e5`）— 資産損失floatバグ・dead-letter・OTPロールバック・pepper 等
- [x] 再公開 **v0.2.1**（R007/R008・frontend修正・メール認証・レビュー反映を束ねた最終バンドル）を GitHub リリース＋HUB 再登録（sha256 `1b54d370`）
- [x] メール送信を **MNN メール（`@*.mnn`）限定**に切替（`MnnMailSender`、外部SMTP廃止） `d6d8645`
- [ ] 本番設定: `MOCHI_MAIL_SERVICE_BEARER`（＋任意 `MOYMOY_OTP_PEPPER`）を運用者が env で設定 — 未設定なら degrade
- [ ] backend 再配置（`deploy-backend.ps1` で v0.2.1 の moymoy-cs を Hub workdir へ）
- [ ] フル E2E（in-world で 0.2.1 再インストール → 口座開設(メール検証)→2FA→リカバリ→送金→チャージ の実機検証）
- [ ] 承認ゲート保留: `MOYMOY_OTP_PEPPER` の本番 fail-closed 化 / `AccountInfo` の email 型統合 / refresh 失敗の UI エラー状態化 / `run_inbound` 切断理由の可視化（mc-sdk 共有層）
