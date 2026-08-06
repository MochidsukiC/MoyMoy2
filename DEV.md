# MoyMoy (MochiOS Mobile 版) — DEV

MochiOS2.0 プラットフォーム向けの電子マネー / ウォレット / 送金アプリ。
完全な先行事例 **PiggleShop2** を最重要テンプレートとして踏襲する3点モノレポ。

実装計画の正本: `C:\Users\dora2\.claude\plans\mochios-mobile-moymoy-mochios-mobile-moy-memoized-puffin.md`

---

## プロジェクト仕様書

- **目的**: MochiOS Mobile 上で動く電子マネー/ウォレットアプリ。通貨単位は「エメ」（小数2桁、台帳は 1/100 エメの整数マイナーユニット。§金額の単位）。MC mod との wire は引き続き物理エメラルドの個数（1エメラルド=1エメ）。
- **構成（3コンポーネント）**:
  - `server/moymoy-cs/` — Rust+axum バックエンド → `moymoy.cs.mnn`。**トンネル内蔵型SDK**(`mochi-hub-cs-sdk` の `CsTunnel::start`)。ウォレットの唯一の権威。SQLite 永続化。**MC mod 無しでも完全動作**。
  - `app-mobile/apps/com.mochi.moymoy/` — HTML/JS バンドル。`fetch("https://moymoy.cs.mnn/...")`。デザイン「MochiOS Mobile.html」駆動。
  - `mod/` — Forge 1.20.1 MC サーバーサイドmod → `moymoy.mc.mnn`。エメラルド消費/付与の真実。connector の `MnnServer` API に `MnnHandler` を実装(`MoyMoyExtension`)、呼び出し元は `MnnRequest#caller()` で `cs:app.moymoy` に固定（旧 `CommandDispatch.Handler` の名前突合は認可でないため廃止）。**オプショナル**。
- **エメラルドチャージ/出金**: チャージはアプリ起点＋ゲーム内 両対応、出金はアプリ起点のみ。双方向コマンドバス（backend が `cs_hosts:["moymoy"]` を claim、`reliable_send` 送信 / `run_inbound` 受信）。
- **整合性**: 「消費の真実=mod / 残高の権威=backend」を `emerald_ops` 台帳 + 二層冪等キー(`idem_key`/`op_id`) + at-least-once 再送 + 冪等決済で eventually-consistent に。
- **方針**: 旧 MoyMoy(`D:\IdeaProjects\MoyMoy`)はドメインの緩い参考のみ。MochiOS2.0 本体は原則無改変（app_backends 配置・mod jar 配置・`mcserver_id` 設定・証明書発行のみ。`hosted_app_ids` は廃止）。

---

## 現在の仕様（デザイン「MochiOS Mobile.html」駆動で確定）

デザインは 電子マネー×クレジットカード風のエメラルド決済アプリ。タブは **home / send(送る) / pay(支払う) / charge(チャージ、内部にチャージ/出金セグメント) / history(履歴)**。ボトムナビ5タブ・ホームのクイックアクション3つは不変。
通貨は「エメ」（小数2桁、§金額の単位）、**9エメラルド = 1エメラルドブロック**（Minecraft、物理個数の換算）。
取引種別 `kind`: `pay`(支払い) / `send`(送金) / `receive`(受取) / `charge`(チャージ) / `withdraw`(出金、引落は負・返金は正)。各取引 `{id, kind, label, amount(符号付), ts}`。
**請求/承認(request/approve)機能はデザインに無い** → 実装しない。

UIフロー:
- **home**: 利用可能残高 + カード(holder/number/expiry) + クイックアクション(pay/send/charge) + 最近の取引4件。
- **send**: フレンド(プレイヤー)選択 → 金額 → 確認 → 完了。残高減・相手は receive。
- **pay**: 近くの加盟店選択 → 金額 → 確認 → 完了、というデザイン当初の直接送金フローは v6 で廃止（`/wallet/pay` 削除、§EC決済）。決済は加盟店が発行する PaymentIntent の承認画面に置き換わった（実装済み、§EC決済）。
- **charge**: チャージ/出金セグメント切替。チャージはインベントリ(手持ちエメラルド + ブロック、9エメラルド=1ブロック)を換算 → 金額 → 確認 → 完了、エメラルド消費し残高加算。出金は金額 → 着金先キャラクター確認 → 完了、残高減で mod がエメラルド付与（§出金整合）。いずれも**MC mod 依存**。
- **history**: 全取引リスト(フィルタ: すべて/支払い/送金/チャージ/出金)。

### アカウントモデル（v2・独立アカウント + PIN）
**独立した MoyMoy アカウント（電子マネー型）**。`account_id` はサーバ生成 UUID で、Minecraft UUID とは独立。

- **資格情報**: `handle`（一意・小文字正規化・`[A-Za-z0-9_]` 3〜20）＋ `PIN`（4〜6桁数字, **Argon2id** ハッシュ保存）。handle は送金宛先（`@handle`）に兼用。
- **セッション**: register/login で 256bit ランダムトークンを発行し、HTTP ヘッダ `X-MoyMoy-Session` で送る。DB には **SHA-256 ハッシュ**で保存（期限 30日・logout で失効）。**backend が全ウォレットリクエストの本人を検証**（旧 mc_uuid 自己申告を解消）。
- **マルチアカウント**: 1端末に複数口座をリンク。クライアント保持リスト（`mochi.storage` / dev は localStorage）が正本で、ヘッダのアバターから切替・追加・ログアウト。サーバは `moymoy_sessions.phone_id` をメタデータ記録のみ。
- **MCキャラ連携（v5）**: 口座↔キャラの永続的な写像は保持しない。どのキャラのエメラルドを操作してよいかはリクエスト毎にユーザー同意付きの Hub 署名 attestation が決める（§出金整合「認可」参照）。`emerald_ops.attester_id` は同意されたサーバーへ再送を届けるためのルーティング情報であり、所有の証明ではない。
- **メール検証 / 2FA / リカバリ（v4、送信基盤は v7 で更新）**: **MNN メール（`@*.mnn`）限定**。送信は `mochi-hub-mailer` の `MnnMailSender` で、認証には launcher が起動時に注入する**このプロセス自身の identity token**（`MOCHI_SVC_IDENTITY_TOKEN`）を使う。旧・共有シークレットの `MOCHI_MAIL_SERVICE_BEARER` は廃止済み（運用者による手動設定は不要）。`MOYMOY_CS_TUNNEL`（既定 `true`）が有効な通常起動では、この identity token が無いとトンネル確立自体が boot 時に fail するため、**launcher 経由の通常運用では常に有効**。トークンが無い状態は `MOYMOY_CS_TUNNEL=0` のループバックのみスモーク等に限られ、そこでは**開設にメール＋OTP必須**の代わりに handle+PIN へ degrade する（`MOYMOY_DEV_OTP_LOG=1` でコードをログ出力するローカル検証モードも同様に degrade 側）。メール有効時はログイン PIN＋メール2FA、PIN 忘れはメール OTP で再設定。OTP は 6桁・SHA-256(+`MOYMOY_OTP_PEPPER`)保存・10分・5回上限・単回・再送クールダウン（`moymoy_otps`）。IPvM ゲートウェイ `/mail/otp-deliver` 経由で相手の in-world メールアプリへ配送、外部SMTPは使わない。`valid_email` は `local@<単一ラベル>.mnn` のみ受理。

### バックエンド HTTP API
全レスポンス `{ok:bool, ...}`。ウォレット系は `X-MoyMoy-Session` でセッション認証（無効は 401）。金額フィールド（`amount` 等）は全経路で 1/100 エメの整数マイナーユニット建て（§金額の単位）。`/wallet/charge`・`/wallet/withdraw` はエメラルドとの往来があるため `%100 != 0` を `bad_amount`（400）で拒否するが、`/wallet/send`・EC決済承認には端数検査が無く 1 マイナーユニット（0.01エメ）から通る。
- `GET /healthz` / `GET /wallet/status` → `{ok, app:"moymoy", can_charge}`（公開）／ `GET /auth/config` → `{ok, email_enabled}`
- `POST /auth/register {handle, display_name, pin, email?, phone_id?}` → メール有効時 `{ok, pending:"verify_email", email}`／無効時 `{ok, session, account}` ／ `POST /auth/register/verify {email, code}` → `{ok, session, account}`
- `POST /auth/login {handle, pin, phone_id?}` → 2FA 時 `{ok, pending:"2fa", email}`／それ以外 `{ok, session, account}` ／ `POST /auth/login/verify {handle, code}` → `{ok, session, account}`
- `POST /auth/recover/start {handle}` → 常に `{ok}`（列挙防止） ／ `POST /auth/recover/verify {handle, code, new_pin}` → `{ok, session, account}`
- `POST /auth/logout`（session） ／ `GET /auth/me` → `{ok, account, email, email_verified}` ／ `GET /auth/lookup?handle=` → 送金宛先解決
- `GET /wallet/home` → `{ok, balance, profile:{holder,number,expiry}, txns:[...recent], can_charge}`
- `GET /wallet/history?limit=&filter=all|pay|send|charge|withdraw` ／ `GET /wallet/friends`（最近の相手・handle 付）／ `GET /wallet/merchants`（`listed=1 かつ status='active'` のみ。デモ加盟店は全て `listed=0` に降格済みなので現状は実質空）
- `POST /wallet/attest/challenge {purpose}` → ワンタイム challenge 発行（`purpose`: `charge`/`session`/`withdraw`）／ `POST /wallet/attest/session {assertion}` → 現在のキャラクターを確認（inventory 読み取り用。消費の認可そのものではなく、チャージ/出金は各々が別途 assertion を要する）
- `GET /wallet/inventory`（引数なし、対象は直前に `/wallet/attest/session` で確認したキャラ）→ `{ok, emeralds, blocks, chargeable}`。失敗: `mc_unavailable`（can_charge=false）/ `attestation_required`（未確認、またはキャラがオフラインと判明）/ `character_unreachable`（mod 無応答）（**MC mod 依存**）
- `POST /wallet/send {idem_key, to_handle, amount, pin?}` → `@handle` 宛 P2P 送金。金額・24h累計・端末一致がリスクベース閾値を超えると `pin` 必須（§リスクベース段階認証）
- `POST /wallet/charge {idem_key, amount, assertion?}` → エメラルド→エメ（着金=account_id / 消費対象キャラ・サーバーは同意付き assertion が決定、§チャージ整合）
- `POST /wallet/withdraw {idem_key, amount, assertion?, pin?}` → エメ→エメラルド（着金先キャラ/サーバーは同意付き assertion が決定、§出金整合）。失敗: `mc_unavailable` / `insufficient`（assertion を求める前に返る）/ `attestation_required` / `bad_amount` / `attest_*`。金銭認証は send と同じリスクベース段階認証を通る
- `GET /wallet/op?op_id=` → チャージ/出金 共通の進捗ポーリング（所有権検証付き）
- `POST /wallet/link {mochi_account_id}` → 入金通知の送り先デバイス登録（session, §入金通知）
- `POST /wallet/_dev/credit {handle, amount}` → dev 専用クレジット（`MOYMOY_DEV_CREDIT=1` ゲート）
- **EC決済（MoyMoy Pay、v6）**: `GET /wallet/payment/intent?intent_id=`（承認画面が表示する唯一の情報源）／ `POST /wallet/payment/approve {intent_id, pin}`（常に PIN 必須）／ `POST /wallet/payment/decline {intent_id}`（PIN不要）。`POST /wallet/pay` は廃止（下記§EC決済）
- **加盟店 API（v6）**: `/merchant/v1/*`（`Authorization: Bearer moy_sk_…`、intent の作成・照会・取消・**履行完了報告**(`intent/fulfill`, v9〜。移動可能な金額はゼロのまま——加盟店が受け取る額を減らす方向にのみ作用する)）／ `/merchant/portal/*`（セッション+PIN、登録・キー再発行・停止・閉店・上限変更・一覧。**例外: `/merchant/portal/sales`（売上ページ、v11）はセッションのみで PIN を課さない**——`portal_list` が既にセッションのみであることとの整合、および読むだけの操作にまで PIN を課すと PIN を何にでも打つ習慣を教えるため）。詳細は§EC決済・§エスクロー。`/merchant/v1` の金額フィールドは **`amount_minor`**（v8）。旧 `amount` キーは `unsupported_amount_unit`、両方無しは `missing_amount_minor`（いずれも400、レート制限・冪等リプレイより前段で弾く）。改名自体が安全機構——キーを使い回すと片側のデプロイ遅延時に単位ズレが検知されずに通ってしまうため（§金額の単位）

### SQLite スキーマ
`accounts`(account_id=MoyMoy口座UUID, handle, handle_lower, display_name, pin_hash, balance, holder, card_number, card_expiry, is_merchant, failed_pin_attempts, locked_until, **email, email_lower(UNIQUE), email_verified**) / `moymoy_sessions`(session_id, account_id, token_hash, phone_id, expires, **mochi_account_id**（v7、入金通知の送り先デバイス。セッション行に持たせ1端末複数口座・1口座複数端末の「最後勝ち」を回避）) / `moymoy_otps`(otp_id, purpose=signup|login2fa|recovery, email_lower, account_id, code_hash, payload_json, attempts, expires) / `transactions`(kind に `withdraw` 追加、引落は負・返金は正) / `merchants`（**v6拡張**: owner_account_id, name_skeleton(UTS#39 confusable skeleton の部分UNIQUE), status=active|disabled|deleted, api_key_hash/prefix/created/last_used, listed(既定0), payer_ref_salt, max_open_intents, daily_issue_cap） / `payment_intents`（**v6新規**: intent_id='pi_'+128bit CSPRNG, merchant_id, amount, description, order_ref, state=created|paid|declined|canceled|expired, payer_account_id, payer_hint_account_id, launch_app_id, tx_id, refunded_unix_ms, refund_tx_id, idem_key, expires_unix_ms。**v9〜v12でエスクロー列を追加**: escrowed_unix_ms（エスクロー到達時刻）・release_due_unix_ms（=escrowed+10分、release sweepの下限）・escrow_deadline_unix_ms（=escrowed+6時間、park判定用）・fulfilled_unix_ms（履行報告時刻、一回きりclaim）・fulfilled_amount（マイナーユニット）・released_unix_ms（release sweepのexactly-onceclaim）・release_tx_id（escrow→merchant）・escrow_refund_tx_id（escrow→payer）・fulfil_reason（v10、120文字上限）・escrow_parked_unix_ms（v12）。詳細は§エスクロー） / `notification_outbox`（**v7新規**: outbox_id, account_id=着金先, kind, label, amount, created_unix_ms, attempts, next_attempt_unix_ms。§入金通知） / `idempotency`(PK=idem_key,scope。出金は scope=`withdraw:<account_id>`、決済は scope=`mi:<merchant_id>` で加盟店ごとに分離) / `emerald_ops`(op_id, account_id=着金先, mc_uuid=消費キャラ, attester_id=同意されたサーバー(再送先を固定するルーティング情報。所有の証明ではない), direction=charge|withdraw, state=pending|sent|settled|failed|stuck, …)。出金は既存カラムに新しい値が増えるのみで**マイグレーション不要**（`kind`/`direction` に CHECK 制約なし）。マイグレーションは user_version ステッパ（v1 baseline `schema.sql` → v2 独立アカウント → v3 1キャラ1口座 → v4 メール/OTP → v5 `account_mc_links` を DROP しキャラクター所有の証明をリクエスト毎の Hub 署名 attestation へ移行 → v6 merchants 拡張 + payment_intents 新設 + デモ加盟店(m1..m5)を listed=0 に降格 → v7 `moymoy_sessions.mochi_account_id` 追加 + `notification_outbox` 新設 → v8 金額列を 1/100 エメの整数マイナーユニットへスケール（`accounts.balance` / `transactions.amount`・`balance_after` / `payment_intents.amount` / `notification_outbox.amount` / `merchants.daily_issue_cap` / `emerald_ops.requested_amount`・`settled_amount`。件数系列（`max_open_intents` 等）・`*_unix_ms` は対象外、§金額の単位） → v9 `payment_intents` にエスクロー列8本追加 + release sweep用部分インデックス `idx_intents_release_due`（`released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL`）新設 + 既存の `state='paid'` 行を移行時刻で `released_unix_ms` 刻印（v9以前は直接決済のため二重払い防止） → v10 `payment_intents.fulfil_reason` 追加 → v11 `accounts.stepup_verified_unix_ms` 追加（riskauthの24h窓アンカー、§リスクベース段階認証） → v12 `payment_intents.escrow_parked_unix_ms` 追加 + release sweep用インデックスを再構築（park済みを除外）。**逆マイグレーションは存在しない**——エスクローは実際に金を移すため、列を消すと保留中の金の行き先情報が失われる。各 `db/schema_vN.sql`）。詳細は `server/moymoy-cs/src/db/`。

### コマンドバス verb（backend→mod / mod→backend ack）
mod との wire は台帳と異なり**物理エメラルドの個数**建て（Java `int`。§金額の単位のアダプタが変換）。
- `emerald.charge {op_id, idem_key, target_uuid, amount}`（amount=物理エメラルド個数） → mod 消費(インベントリのエメラルド+ブロック) → ack `{op_id, status, settled:consumed}`
- `emerald.withdraw {op_id, idem_key, target_uuid, amount}`（amount=物理エメラルド個数） → mod がエメラルド付与(`amount/9`ブロック+`amount%9`個、満杯時は足元ドロップ) → ack `{op_id, status, granted}`（`settled` とは別名。status: `ok`/`duplicate`(再付与せず同額を再ack)/`unknown`(claim済みだが付与を証明できず`stuck`・返金なし)/`player_offline`/`bad_request`/`unauthorized`/`internal_error`）
- `inventory.query {req_id, target_uuid}` → mod が手持ちを返答 `{req_id, emeralds, blocks}`（charge画面のインベントリ表示用。任意）

### 出金整合とattestation（§出金整合、チャージの逆方向）
出金 = エメ残高 → ゲーム内エメラルドで受け取る。チャージは「先に消費 → ack で入金」、出金は「先に引落(予約) → 付与要求 → ack で確定」と安全な失敗の向きが逆転する（付与を先にするとエメラルドが無から増えインフレする）。
- 確定的失敗（`player_offline` 等・付与なし）は同一トランザクション内で即返金。付与されたか**不明**（`unknown`）な場合は返金しない（返金するとエメラルドと残高の両取りになる）ため `stuck` にして手動レビュー（R008 と同じ規律）。返金は state を非終端から動かした当のトランザクション内でのみ行い、UPDATE の変更行数でガードして reconcile と ack の同時到達による二重返金を構造的に防ぐ。
- `reconcile` の dead-letter は方向別: チャージの一括 UPDATE には `direction='charge'` の絞りが入り、出金は1行ずつ処理（未送達 `pending` → 返金して `failed` ／曖昧な `sent` → 返金なしで `stuck`）。再送は台帳の `direction` で verb を分岐（出金 op をチャージとして再送するとプレイヤーのエメラルドを逆に没収するため必須）。
- **認可**: `AttestPurpose::Withdraw`（purpose 文字列 `"withdraw"`）と request-hash ドメイン `moymoy.withdraw.v1` を新設。ドメイン分離＋challenge の purpose バインドにより、チャージ用 assertion で出金はできず逆も不可。
- **上限**: 1操作あたり 20,736 エメ（=2,304 エメラルドブロック=インベントリ1個分）を backend（`MAX_WITHDRAW_PER_OP`、マイナーユニット建て）と mod（`MAX_WITHDRAW_PHYSICAL`、物理個数建て）が単位の異なる別々の定数として独立に強制（無制限だと mod 側で数百万スタックが生成されサーバースレッドを固めるため）。
- **mod 側冪等**: `grants` コンパウンド（`op_id → -1`=claim済み未確定／`≥0`=確定付与額）で管理。チャージ用の `ops` は不変・後方互換。claim → ディスクへ同期フラッシュ → 付与 → 確定記録 → フラッシュ、の二相（付与は無から生成するため記録前クラッシュ＋リプレイでの二重生成を防ぐ必須要件）。判定〜claim〜付与〜確定は1回の `server.submit` 内で完結させ、同一 op の同時実行が両方とも「未 claim」を観測することを構造的に不可能にしている。
- **不変条件の変更**: 従来「ウォレットから価値が外へ出る経路は無い」が成立していたが、出金ではこれは成立しない。代わりに ①出金は必ずセッション本人の操作、②金額と `idem_key` に束縛されたユーザー同意付き assertion を要する、③`AttestedFacts::account_id` は依然として認可に読まれない、④attester（サーバー運営者）は他人の残高を動かす手段を持たず宛先キャラを名乗れるのみ、が担保される。新たな信頼境界は「ユーザーが出金先 MC サーバーを信用する」こと。

### EC決済（MoyMoy Pay、v6・エスクロー v9〜v12）
MochiOS 内の EC サイトが MoyMoy で決済する仕組み。VISA 3D Secure と同型のリダイレクトフロー。**第三者開発者も使う公開 API**（第一号加盟店 PiggleShop2）。**第三者開発者向けの入口は `docs/merchant-quickstart.md`**（登録→キー→intent作成→起動→照会確定の最短ルート）。

フロー: EC バックエンドが API キーで PaymentIntent を作成 → EC アプリが `os.apps.launch("com.mochi.moymoy", {intent_id})` で MoyMoy を起動（渡すのは `intent_id` のみ）→ MoyMoy が自バックエンドから intent 詳細を取得（`GET /wallet/payment/intent`）して承認画面を表示 → PIN → `POST /wallet/payment/approve` が同一トランザクションで intent claim + **支払者からの即時引き落とし**（送金先は加盟店ではなく MoyMoy 本社のエスクロー口座、§エスクロー） → EC へ戻る → **EC バックエンドが moymoy-cs へ照会して `paid` を確認してから履行**（クライアントの「払った」申告は信用しない）→ **履行完了後、EC バックエンドが `POST /merchant/v1/intent/fulfill` で結果を報告し、履行完了報告＋10分経過をもってエスクロー口座から加盟店へ送金・差額は買い手へ返金される**（§エスクロー）。買い手の体験・承認画面・PIN 要求は無変更。

不変条件（出金で確立したものを移植）: ①資金移動は必ずセッション本人の操作で、**API キーではいかなる残高も動かせない**（intent の作成・照会・取消・履行完了報告のみ、履行完了報告は受取額を減らす方向にのみ作用） ②承認画面が表示する金額・加盟店名は moymoy-cs の記録が唯一の真実で、クライアント渡しで信用するのは `intent_id` のみ ③第三者は「宛先」を名乗れるだけで「誰の財布が払うか」は名乗れない ④資金移動は `state='created' AND expires_unix_ms > now` を条件とした確定的 UPDATE 一文（変更行数==1のときのみ transfer へ進む）。この `now` は**確定処理の内部で読む**（承認ハンドラの入口から受け取らない） — 到着時刻を使うと Argon2id 比較やロック待ちで数百ms 空くため、PIN 検証中に期限が切れても送金が成立してしまう。**エスクロー導入後もこの一文は無変更**——変わったのは transfer の宛先（加盟店→エスクロー口座）とその後に書き込まれる3つのタイムスタンプだけ。

intent の状態機械: `created → paid / declined / canceled / expired`。全終端は最終（`paid` はリファンドでも巻き戻らず、逆方向の別トランザクションになる）。二重承認・期限切れ直前承認・加盟店キャンセルとの競合は上記 UPDATE 一文で排他される。承認前チェックとして `merchants.status='active'` を必須、`payer_hint_account_id` 指定時は他口座からの承認/拒否を `payer_mismatch` で拒否、リプレイ（`paid` かつ同一 payer）は PIN 検証より前に応答して試行回数を消費しない。**`escrow_stage`（`none`/`held`/`parked`/`fulfilled`/`released`）は上記 `state` とは別軸**——`paid` で `state` は最終になるが、資金の所在を表す `escrow_stage` はそこから `held → fulfilled → released`（または `held → parked`）と動き続ける（§エスクロー）。

加盟店: セルフサーブ登録。1口座あたり保有できる加盟店は `MAX_MERCHANTS_PER_ACCOUNT`（3件、`status != 'deleted'` を数える。停止(disabled)中も枠を占有し続ける — 名前とスロットは同じ予算で、`close` 以外に返す手段が無いため）。API キーは `moy_sk_` + 256bit CSPRNG、DB には SHA-256 ハッシュのみ保存、平文は登録/rotate 応答で一度だけ返す。`/merchant/v1/*`（API キー認証、intent の作成・照会・取消・履行完了報告。移動可能な金額はゼロ）と `/merchant/portal/*`（セッション+PIN、登録・キー再発行・停止・閉店・上限変更・一覧。売上ページ`/merchant/portal/sales`のみPIN不要）の2系統分離。加盟店名は UTS #39 confusable skeleton で一意化（`lower`+NFKC だけでは別スクリプトの同形異字 `PiggleShoр2`(Cyrillic er) が素通りする）し、スクリプト混在を拒否、運営語彙（`moymoy`/`公式`等）を予約語として禁止。**リネーム API は存在しない**（登録時固定）。`description`/`sub`/`name` は NFKC 後 Cc+Cf（bidi制御・ZWJ含む）+未割当+私用領域を拒否し結合文字を基底1文字あたり3個までに制限（`U+202E` 等での承認画面偽装を防ぐ）。**登録のレート制限は2段構え**: 実際に店が作られた（＝有効な名前・PIN・空き枠がすべて揃った）ときだけ10分に1回の枠を消費する（名前の打ち間違い等の失敗では消費しない）。別に、試行そのもの（成否問わず）を絞る5回/10秒のバースト枠があり、これはセッションを握った攻撃者が無効な名前を連打してPIN照合のCPUを消費させる手口を防ぐためのもの。

**閉店（`POST /merchant/portal/close`、セッション+PIN）**: `status='deleted'` の soft delete（`name_skeleton`/`api_key_hash`/`api_key_prefix` を NULL・`listed=0`）で、名前・資格情報・枠を解放しつつ台帳（`payment_intents.merchant_id` の参照先）は残す。行ごと削除すると「一度でも取引した店は閉じられない」か「注文履歴を道連れにする」のどちらかになるため。**未決済 intent が1件でもあれば拒否される**（`open_intents`、count 付き）。閉じた加盟店は再オープン不可（登録し直す）。

**加盟店の発行上限**（エスクローとは独立に、加盟店による intent 発行速度そのものを制限する防御策）: 未決済 intent 件数上限（既定20、上限500）と 24h 発行合計金額上限（既定50,000エメ、上限2,000,000エメ）。引き上げは `/merchant/portal/limits`（セッション+PIN）のみ、API キー単体では不可。

### エスクロー（決済の保留と解放、schema v9〜v12）
v8までは承認と同時に加盟店へ直接送金していた（`settle` が payer→merchant を単一トランザクションで転記）。v9以降はその間に MoyMoy 本社の保留口座を挟む:

```
買い手が承認 → 支払者の口座から【即時】引き落とし（買い手の体験は不変）
             → MoyMoy 本社のエスクロー口座で【保留】
             → 加盟店が履行完了を報告し、かつ10分経過（`RELEASE_GATE_MS`） → 加盟店へ送金＋差額を買い手へ返金
```

**時間ではなく履行完了通知でゲートするのが要点**。配送に数時間かかっても資金が本社に留まり続けるため、返金の原資が常に存在する（v8以前は集金直後に加盟店が MC 世界へ出金すると原資が消える、という受容リスクがあった。§問題）。

- **エスクロー口座**: `wallet::seed_escrow_account` が決定論的 `account_id` の非ログイン口座（`handle`/`pin_hash` とも NULL）を `INSERT OR IGNORE` **単体**で冪等生成する。デモ加盟店の `seed_demo_merchants` にある件数ガードは**真似ていない**——既存 DB では `merchants` に行があるため件数ガードを付けると口座が永久に作られず、`wallet::transfer` が全決済で `UnknownTarget` になる。
- **履行完了報告 API**: `POST /merchant/v1/intent/fulfill {intent_id, fulfilled_amount_minor, reason?}` → `{ok, state:"fulfilled", fulfilled_amount_minor, refund_amount_minor}`。`fulfilled_amount_minor` は `0 <= x <= intent.amount`（`0`=全額返金という有効な値なのでフィールド欠落は400で拒否、`Option<i64>` で受ける）。claim は `fulfilled_unix_ms IS NULL` で一回きり——2回目は 409 `already_fulfilled` ＋最初の額（最初の報告だけが買い手の返金額を決める）。不明・他店は 200 `{ok:false, error:"unknown_intent"}`（`cancel` と同じ扱い。409にすると intent の存在が漏れる）。**実送金はここでは起きない**——10分ゲート後に release sweep が動かす。`reason`（`fulfil_reason`、v10）は加盟店の売上ページに描画されるため **120文字上限・`sanitize_text` 通過必須**（超過は400で拒否、切り詰めない）。
- **release sweep**: 既存の30秒ループに相乗り。**1行ずつ claim**（金銭移動の義務を伴うため一括UPDATEにしない）。`fulfilled_amount` の加盟店送金と差額の買い手返金は**同一トランザクション**。走査対象は部分インデックス `idx_intents_release_due`（`released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL AND escrow_parked_unix_ms IS NULL`）。
- **未報告のエスクローは6時間後に park される。自動返金はしない。** 理由: PiggleShop の `delivery_deferred`（インフラ障害時は配送リトライ予算を消費しない設計）により配送の足踏みに上限が無いため、6時間で自動返金すると「正常に配送されるはずだった注文が返金され、買い手が商品と代金の両方を持つ」経路が実在する。時間の経過は「商品が出なかった」ことの証拠にならない——出金の R008（付与されたか不明なら返金せず `stuck` にする）と同じ規律。
- **park は `released_unix_ms` を書かない。** 下記「運営の強制返金」の `force_refund` が `escrowed_unix_ms.is_some() && released_unix_ms.is_none()` を選択条件にしているため、これが park された金を買い手へ返す唯一の経路。park 済みを「解決済み」として扱う変更を入れると金が宙吊りになる。
- ⚠ **park された金を加盟店へ払い出す経路は存在しない**（実装していない）。運用者にできるのは買い手へ返すことだけ（§問題）。
- **`escrow_stage`**（`payments::escrow_stage`）: `none`（未決済、またはv9移行前の決済=既に入金済み）/ `held` / `parked` / `fulfilled` / `released`。v9マイグレーション時点で `state='paid'` の既存行は全て `released_unix_ms` を移行時刻で刻印済み（`escrowed_unix_ms`はNULLのまま）——v9以前の直接決済は既にエスクローを経ずに加盟店へ着金しているため、release sweepの二重払いを防ぐ。

### 加盟店売上ページ（v11）
`GET /merchant/portal/sales?merchant_id=&limit=`（**セッションのみ・PIN なし**。理由は「バックエンド HTTP API」§加盟店 API 参照）。

`held_total_minor` は `payment_intents` からの**導出値**——残高列は足していない。実在する金（エスクロー口座）を追う2つ目の数字を作ると、いつか食い違う。エスクロー口座の残高から「この店の分」は個別に答えられない（全店の金が1つの壺）。

`limit` は既定50・上限200。**`truncated` を返す**。並び順は `created_unix_ms DESC, rowid DESC`（同一ミリ秒のタイで順序が不定だと、`truncated` が「切れた」と言いながら何が切れたか決まらない）。

**運営の強制返金**: `moymoy-cs admin refund <intent_id> [reason...]`（CLI サブコマンド）で実装済み。`paid` は巻き戻さず、逆方向の別トランザクションとして記録する。**HTTP エンドポイントには出さない** — 他人の口座から同意なく金を動かす最も強い権限操作なので、盗まれたセッション・漏れた API キー・ルーティングのバグから守る手段は、そもそもネットワーク面を作らないことにした。**park された金（履行未報告のまま6時間経過したエスクロー）を買い手へ返す唯一の経路でもある**——選択条件は上記「エスクロー」参照。

**シェルの attestation モーダルは決済承認に使わない**（決定）。MochiOS の `docs/developers/host-attestation.md` §3 が明示的に禁止しており、`reason`（ユーザーに見える文言）は署名クレームに入らず `request_hash`（署名される値）はユーザーに見えないため、見たものと署名したものが構造的に一致しない。承認は MoyMoy アプリ内の PIN が担う。※MochiOS 側ソースは未照合（moymoy-cs リポジトリからは検証不能）。

OS 依存（決定、未照合）: MochiOS の `os.apps.launch` / `os.apps.takeLaunchIntent`（Phase 0 実装済とされる）。ウォームスイッチではクエリが URL に載らず失われるため、`intent_id` はホスト側メールボックス経由で渡す。

### リスクベース段階認証（riskauth、v6）
機能別に PIN を撒かず、資金流出の唯一の関門にする。`/wallet/send`・`/wallet/withdraw`・決済承認 (`/wallet/payment/approve`) が同じ関門 (`riskauth::step_up`) を通る。`/wallet/charge` は資金流入なので対象外。

3段階の閾値（コード定数、環境変数化はしない。実運用で調整する前提。値はエメ単位——コード内部の定数はマイナーユニット建てのため見た目は ×100 されているが、v8 の単位移行は閾値そのものを変えていない、§金額の単位）:
- 単発 **200 エメ以下**かつ 24h 累計流出 **1,000 エメ以下**かつ端末一致 → **認証なし**（`FRICTIONLESS_*`、無変更）
- それ以外 → **PIN**
- 単発 **10,000 エメ超**（`STEPUP_SINGLE`）、または 24h 累計流出 **100,000 エメ超**（`STEPUP_DAILY`）、または**端末不一致** → **PIN + メール OTP**

**24h累計流出の窓は固定のローリング24時間ではない**（schema v11、`accounts.stepup_verified_unix_ms`）: `max(now - 24h, stepup_verified_unix_ms)` 以降の流出を集計する——OTPで既に認証済みの金額を「まだ本人性が怪しい」側の総量に数え続けると、日次上限を一度超えたアカウントが以後のあらゆる小額決済にまでOTPを要求され続けてしまうため。**リセットは OTP 検証が成功した瞬間**（送金・決済の確定後ではなく、コードの消費と同一トランザクション）。**PIN だけで通った場合はリセットしない**——PIN はセッションと一緒に移動するため、守られている側が自分の門を開けることになる。集計は `transactions` の**負の行すべて**（`kind` フィルタなし。送金・支払い・出金が合算される）。

決済は既定で常に PIN 以上（`Requirement::Pin` が floor で、金額・端末不一致はそれをさらに引き上げることしかできない）。端末一致は `moymoy_sessions.phone_id`（account の最古の phone_id 保持セッションとの照合、一度も device id を送っていないアカウントは既存ユーザー保護のため常に「一致」扱い）。**この列は v6 以前は保存されるだけで一度も照合されていなかった。**

メール OTP を要求する第三段階で、口座にメール未検証（または deploy 側でメール未設定）の場合は **PIN だけで通さず拒否する**（`otp_unavailable`）。閾値が「決めない」状態を作らないための fail-closed。

**OTP の検証は送金・決済の確定トランザクションとは別のトランザクションで、両方の結果で必ず commit する。** 畳んで確定処理の中で検証すると、コード誤り時にロールバックで OTP の失敗カウンタごと巻き戻ってしまい、6桁コードへの総当りに5回の上限が掛からなくなる（一度実際に起きた不具合）。正しい PIN・誤った OTP の場合は PIN の失敗カウンタだけ払い戻す（OTP を打ち間違えただけで口座がロックされないように）。この分離は「後で統合した方が綺麗」に見えて壊れる箇所なので、理由ごと維持する。

PIN 検証は「短いトランザクションで失敗カウンタを先に書いて commit → トランザクション外で Argon2id → 短いトランザクションでロックアウト再検証しカウンタをクリア」の3段構え。単一トランザクション内で Argon2id を回すと SQLite の単一ライタロックを数百ミリ秒保持し、ウォレット全体が停止するため。PIN が正しかったが操作自体が成立しなかった場合（残高不足等）は `refund_attempt` で消費した試行を返却する（さもないと残高不足への正しい PIN リトライ5回で自分の口座をロックする）。

`/wallet/pay`（デモ加盟店ハードコードの直接送金経路）は PaymentIntent が置き換えたため**削除済み**。デモ加盟店(m1..m5)は全件 `listed=0` に降格し、`GET /wallet/merchants` は `listed=1 AND status='active'` のみを返す。

### 入金通知（OS push、v7）
残高が増える全経路（P2P受取・チャージ確定・**加盟店売上（エスクロー解放時、履行完了報告＋10分後。v9〜承認と同時ではない）**・**エスクロー差額の買い手返金（v9〜）**・出金返金・dev credit・CLI強制返金）は `wallet.rs` の `transfer` 受取側と `credit` の2箇所に集約されており、両方とも同一トランザクション（`tx`）内で `queue_deposit_notification` を呼び `notification_outbox` 行を書く。行はその commit と運命を共にする（rollback されれば行も残らない）ため「通知が届いたなら入金は commit 済み」が保証される。逆方向（入金したら必ず通知が届く）は best-effort のため成立しない（後述）。

`src/notify.rs` の配送タスクが2秒ポーリングで outbox を drain し、MochiOS notifications サービス（`http://127.0.0.1:7406`、定数直書き・env新設なしは承認済み決定）へ `MOCHI_SVC_IDENTITY_TOKEN`（前掲、launcher 注入の per-process identity token）bearer で POST する。配送は best-effort: 失敗は 5s/10s/20s/40s のバックオフで最大4回リトライし、5回目の失敗で行を破棄（一時的な超過は吸収するが、持続的な失敗では通知が落ちる。ただし台帳=`transactions` は無関係で影響を受けない）。outbox 行が解決できない場合（該当口座が消えている等）も配送失敗と同じ経路で行単位に隔離して age out する。1パスの結果反映（delete/update）は1トランザクションにまとめている。`MOCHI_SVC_IDENTITY_TOKEN` 未設定時は degrade し、行を配送せず破棄する（outbox の無限成長を防ぐ）。

端末リンク: schema v7 で `moymoy_sessions.mochi_account_id` を追加。`POST /wallet/link {mochi_account_id}`（セッション認証、mail の `/mail/link` と同じ自己申告モデル。金銭認可には一切読まれない）。セッション行に持たせることで1端末複数口座・1口座複数端末の「最後勝ち」を回避し、logout の行削除・期限切れで自然消滅するため unlink API は不要。

通知内容: title「入金」、body「@handle: {取引ラベル} +N エメ」、action_uri で MoyMoy 起動（`mochi-internal://com.mochi.moymoy/index.html`）、category "wallet"。見た目は OS 仕様上「M＋ハッシュ色」（icon_asset は OS シェル未実装）。

アプリ側: `moymoy-sdk.js` に `mochiOwner()`/`link()` を追加、`moymoy-auth.jsx` の3箇所（ログイン成功/boot復元/口座切替）で fire-and-forget 登録。browser-dev はスキップ。

MochiOS 側（別リポジトリ、未照合）は `ALLOWED_PUSH_SENDERS` allowlist を廃止し、`app.<name>` identity と notification.app_id の末尾ドットラベル一致の構造束縛＋action_uri 束縛＋送信者別レート制限（120件/60秒、band対象外）に置換したとされる（決定・未照合、mail の `MAIL_CALLER_SET` 撤廃と同型の設計）。

### 金額の単位（v8、本番適用済）
台帳の金額は**1/100 エメの整数マイナーユニット**。整数のみで浮動小数は使わない（残高は和であり、二進浮動小数は十進小数を正確に足せないため）。×100 された列は上記 SQLite スキーマの v8 移行を参照。

**mod との wire は引き続きエメラルドの個数**（mod の `amount` は Java `int` のため、マイナーユニットを流すとオーバーフローする——これは方針ではなく wire の制約）。変換は mod 対向のアダプタに集約されている：
- `mc.rs` の `to_physical`（`send_charge`/`send_withdraw` が使用） — minor → 物理（÷100）。`%100` と範囲を検査し、**丸めも切り詰めもせず** 500 で落とす。
- `charge.rs` の `ack_amount` — 物理 → minor（×100）。mod の報告を取り込む**唯一**の関数。
- `charge.rs` の `chargeable_minor` — 物理 → minor。

`MINOR_PER_EMERALD`（mod 側の物理換算用）と `MINOR_PER_EME`（台帳の通貨定義）は**別定数**（今日時点の値は一致しているが、将来の乖離に備えて意図的に分離）。

`emerald_ops.requested_amount`/`settled_amount` の2列も ×100 している。`settled_amount` は `credit_charge` の `credited`（台帳の値）をそのまま保持しており、物理個数のまま残すと**スキーマ内で単位が違う唯一の列**になるため。安全性の根拠は「変換点を全部数え上げたこと」ではなく「**変換を `ack_amount` の内側に集約したこと**」——呼び出し側で変換する実装だったら `charge/withdraw.rs` の `settled_amount` 書き込みが物理個数のまま残り、settled 行だけ単位がズレていた。

**単位の検査**: `api.rs` の `whole_emeralds()` — チャージ・出金は `%100 != 0` を **400** で拒否（エメラルドは不可分）。送金・決済承認には `%100` 検査が**無く**、1 マイナーユニット（0.01エメ）から通る。

### デプロイの制約
**MoyMoy と PiggleShop2 は同時にしかデプロイできない**。`attest` の署名ハッシュと `amount_minor` 契約が、MoyMoy サーバー・MoyMoy アプリ・PiggleShop2 サーバー・PiggleShop2 クライアントの4成果物に跨るため。加えて v9〜v12（エスクロー・履行完了通知API）は MoyMoy-cs と PiggleShop2-cs 間の契約にも跨る。**逆マイグレーションは存在しない**——エスクローは実際に金を移すため、列を消すと保留中の金の行き先情報が失われる。**今回は PiggleShop2 側の MC mod が変わる（配送 claim を叩くようになる）ため、Minecraft サーバーの停止と mod 入れ替えが必要**（従来のv8リリース時点では「Minecraft サーバーと mod は停止も変更も不要」だったが、今回は当てはまらない。MoyMoy 自身の Forge mod は無変更）。**v9〜v12はコミット済み・本番未適用**。現在のアプリバージョン: **0.6.0**。**アプリの GitHub リリースはアプリストア公開より前に作る必要がある**——インストーラは manifest の `bundle.url` を見て直接 GitHub から取得し、ローカルのリポジトリは見ない（`MochiOS2.0/mobile/cef-host/src/app_install.rs:188-189`）。

---

## 問題 / 課題

- **本人検証の到達点**: v2 で「自己申告 mc_uuid」→「backend が検証する handle+PIN セッション」へ移行し、ウォレットの本人性は MoyMoy 内で完結して検証可能になった。MochiOS のゲートウェイは cs.mnn 宛に検証済みアカウントを注入しない（調査確認済）ため、OS アカウント連携ではなく **MoyMoy 独自資格情報**で本人を担保している。
- **セッショントークンの保存**: クライアントは `mochi.storage`（in-world は per-app 隔離・再起動跨ぎ永続）/ dev は localStorage にトークンを保持。盗用時の被害は当該口座に限定され、logout・期限切れで失効。より強固にするなら端末バインドや短命トークン+リフレッシュを将来検討。
- **memo 未実装**: デザインの送金/支払いフローに memo 入力欄が無いため、API からも除外（受理して捨てる挙動は不採用）。必要時は transactions.memo への配線を追加。
- **in-game チャットコマンドからの backend 報告は不可**: `mochi` connector(`MochiMod`)は `DISPATCH`(inbound ルーティング)のみ公開で、ハンドラ外からの unsolicited 送信API が無い。よってエメラルドチャージは**アプリ起点**で完結する。真の `/eme deposit` には mochi connector への outbound 送信API追加（承認の要る MochiOS2.0 改変）が必要。
- エメラルドチャージの致命ウィンドウ（consume成功・ack喪失・SavedDataフラッシュ前クラッシュ）は台帳+reconciliation+`setDirty()`直後フラッシュで最小化（exactly-once は原理的限界）。
- **出金 mod 側フラッシュのベストエフォート性**: `EmeraldOpStore.flush()` はベストエフォート。バニラの `SavedData#save(File)` は書き込み失敗をログのみで飲み込み dirty フラグを無条件でクリアするため、書き込み失敗直後にクラッシュすると claim が失われリプレイで二重付与が起こりうる（mod 側に検知手段なし）。
- `.dat` 破損時にバニラの `DimensionDataStorage#get` が例外を握り潰しストアを丸ごと空扱いにする窓（`ops`/`grants` 共通、チャージにも元からある）。
- **出金の信頼境界**: 出金先 MC サーバーへの信用が新たに発生する。悪意あるサーバーは `ok` を返しつつ実際には付与しないことができ、機能の性質上これは回避ではなく同意の上で受容するリスク。
- **決済の信頼境界（エスクロー導入で解消。新たな残存リスクに置換）**: v9〜v12のエスクロー導入により、加盟店の売上は承認と同時にはMoyMoy本社のエスクロー口座に保留され、加盟店へは履行完了報告後10分経過してはじめて送金される。従来ここに記載していた「加盟店売上を拘束しないため詐欺加盟店が集金直後にwithdrawで抜けると回収不能になる」リスクは解消済み。残存するのは別種のリスク: **park された資金（履行未報告のまま6時間経過したエスクロー）を加盟店へ払い出す経路が存在しない**——運用者にできるのは買い手へ返すことのみ（§エスクロー）。**park を知る手段は `warn!` ログとDB直接参照、および加盟店自身の売上ページのみ**——運用者向けの一覧は無い。
- **`LockoutPolicy::Bypass`（加盟店の緊急停止）の増幅**: `/merchant/portal/status` で shop を disabled にする操作は口座ロックアウト中でも PIN を試させるが、`begin_pin_attempt` は Bypass でも `locked_until` への書き込み自体は行う（拒否判定だけスキップする）。セッションを奪った攻撃者が被害者の他エンドポイントを誤 PIN で継続的にロックできる増幅がある。auth の意味論変更になるため未対処。
- `riskauth::outflow_24h` は `transactions` の負の amount を全て合算するため、返金済みの出金も 24h 枠を消費し続ける（摩擦が増える安全側の歪み）。
- PIN のロックアウトカウンタ（`accounts.failed_pin_attempts`、5回で15分ロック）はログインと共有のため、handle を知る第三者が誤 PIN 5回で被害者の送金/出金/決済を15分単位で止められる。決済・送金・出金側はセッション単位の指数バックオフ（`PinBackoff`）を併用して緩和している。
- **端末リンクは自己申告（§入金通知）**: セッション保持者が他人の Mochi UUID を `POST /wallet/link` に登録すると、自分の入金通知をその端末に送りつけられる。金銭的な影響は無く、mail の `/mail/link` と同じ割り切り（受容済み）。
- **`auth::valid_display_name`（v4以来の既存欠陥）**: `char::is_control()`（Unicode Cc のみ）で検証しており、Cf（`U+202E` 等の bidi 制御・ZWJ 含む）が素通りする。表示名は入金通知の本文・アプリ内履歴の両方に流れるが、24文字上限と OS シェルの HTML エスケープにより injection は不可で、表示順序偽装の余地のみ。merchant 名の v6 パイプライン（NFKC 後 Cc+Cf を拒否）と非対称。
- MochiOS2.0 の DEV.md への今回の通知認可再設計（`ALLOWED_PUSH_SENDERS` 廃止等）の記録は、同リポジトリに別セッションの未コミット変更が存在するため保留中（コミット済みコードの doc コメントには記載済み）。
- v0.5.1 の manifest bundle sha256/size が GitHub 実資産と不一致だった（ストアインストールが整合性検証で失敗していた可能性）。v0.5.2 で解消済み（sha256 = `d733b0a1…`、259072 bytes、manifest と一致検証済み）。
- 加盟店の `launch_app_id` は intent 作成時の自己申告で、その app_id が本当にその加盟店のものかを moymoy-cs は検証できない。承認画面での OS 由来 `from` との不一致警告は実装済みだが、これは表示レベルの注意喚起であり、`launch_app_id` の真正性を moymoy-cs 側で検証する仕組みではない。
- `schema_v6.sql`/`schema_v7.sql` のカラムコメントに `-- エメ` が残っている。適用済みマイグレーションは書き換えない方針によるもので、当時の記述としては正しい。単位の現在値は §金額の単位（v8）の記述が正。
- `clippy -D warnings` が3件失敗する（`PooledConn` 未使用 / `identity::Account` のフィールド未読 / `CreateOutcome` の `large_enum_variant`）。いずれも単位移行（v8）以前から存在する既存警告。
- `merchants.daily_issue_cap` / `max_open_intents` / `notification_outbox` は本番に実データが無いため、v8 移行の指紋比較では正しさを検証できていない。担保はコード側のテストのみ（`db::tests::migration_v8_scales_amounts_and_leaves_counts_alone`）。
- **CodeX 再レビュー（反映済）**: v2 再設計に recursive-codex-reviewer を実施。妥当指摘を反映 — backend `382acc2`（冪等の複合PK化で二重決済防止 / `user_version` を tx 内へ移しマイグレーション原子化 / 握り潰しログ化 ほか）、frontend `ffb40c8`（`me()` を ok/expired/unknown で識別し一時エラーで口座を消さない / アンマウントガード / 401 即時処理 ほか）。
- **承認ゲート保留（共有層に跨る設計課題・未着手）**: 着手前に設計案の承認が必要。
  - **R008**: `reconcile` の op TTL / dead-letter。`sent` の消費済みエメラルドを安全に失効させる escalate フロー（単純 TTL は消費済み無クレジット化の危険）。
  - **R05/R06**: SDK の `_session` がグローバルのため、非アクティブ口座の logout / 切替検証中に並行 API が誤セッションを送る競合。`getJson/postJson` への per-call トークン引数化で根治。
  - **R13 / charge 再試行**: `store.set` 失敗の握り潰し、チャージ poll タイムアウト後の再試行で別 op_id の二重消費窓。

---

## [2026-08-05] 夜間セキュリティ/バグ監査

資金移動・元帳整合性・認証/OTP/リスク判定・SDK を対象に多エージェント監査。各指摘は反証専任の検証者2名（到達可能性・悪用可能性、「確信が持てなければ反証扱い」の指示付き）を通し、両方が反証できなかったものだけを載せている。棄却された指摘は書いていない。

**修正済み（`3cabd87`）【CRITICAL】**: `app-mobile/apps/com.mochi.moymoy/moymoy-sdk.js` の `base()` が dev 用の上書き `?moymoy_http=` を in-world 判定より先に読んでいた。別アプリが `mochi-internal://com.mochi.moymoy/index.html?moymoy_http=<攻撃者>` へ遷移させるとクエリは cold start まで生き残るため、本物の MoyMoy が攻撃者のバックエンドを向いて起動する。見た目は普通のログイン画面のまま、入力された handle と PIN、ログイン済みならセッショントークンが攻撃者に渡る。manifest の `allowed_origins` は防御にならない（MochiOS 側でそれを強制しているのは `mochi.api.call` だけで、この SDK は生の `fetch` を使う。この層の欠落自体は MochiOS2.0 の DEV.md に承認待ちとして記録した）。

**監査中に別途修正されたもの**: 本監査は `api.rs:1330` の `withdraw_gate` が attestation 不在を検出する前に単回使用 OTP を消費してしまい 5,000 エメ超の出金が絶対に成功しない件を検出したが、検証中に `bd69589` で独立に修正されていた（peek / step_up 分離）。重複対応はしていない。

### 未修正 — 要判断

- **【MEDIUM・承認ゲート】`server/moymoy-cs/src/riskauth.rs:256,275` — step-up の判定が資金移動トランザクションの外側で1回しか行われない。**
  24時間累計（`outflow_24h`）をトランザクション外で読み、`riskauth::settle` はロックアウトしか再確認しないため、並行リクエストが全員「移動前」の累計を読む。セッションと PIN を握った攻撃者（メールは受け取れない）が同額のリクエストを同時に多数投げると、全部が `Requirement::Pin` と判定され、`STEPUP_DAILY`(100,000) のメール OTP 要求を迂回できる。被害は青天井ではなく、接続プール `max_size(8)` が並行度の上限になるので概ね「8 × Pin 帯の上限額」で頭打ちになり、以降はコミット済み累計を読んで正しく OTP を要求する。出金は 1 op ごとに Hub 署名済み assertion が要るぶん更に弱く、実際に効くのは主に `/wallet/send`。
  **判断が要る点**: `send` と `approve` は `riskauth::settle` が既に money tx 内にあるので、`StepUpTicket` に「クリアした Requirement と判定に使った額」を持たせて tx 内で `assess_for` を再実行すれば閉じる。しかし **withdraw は閉じられない** — `withdraw_gate` は自分の tx を commit して接続を返した後、非同期の attestation 往復を挟んでから `charge::begin_withdraw` の別 tx で reserve する（最も広い窓）。ここを塞ぐにはチケットを `api.rs → charge.rs → charge/withdraw.rs` へ引き回し、`api.rs:1317-1321` が明文で置いた「このゲートは charge モジュールのトランザクションに手を出さない」という設計判断を覆す必要がある。**send/approve だけ直して「解決済み」にすると、不可逆な in-world 払い出しを伴う withdraw が racy なまま残る**ので、3経路まとめて設計してから着手すること。なお同じ形の 24h 上限を正しく tx 内で扱う実装が `merchant.rs:712-748` の `check_issuance(tx, …)` にあり、参考になる。

- **【MEDIUM】`server/moymoy-cs/src/api.rs:937` — `/wallet/send` の冪等キーが呼び出し元でスコープされていない。**
  グローバルな `"send"` スコープで記録・参照するため、ある口座の `idem_key` が全口座のものと衝突する。`/wallet/charge` では `charge_scope(account_id)` を導入し schema v5 で移行してこの欠陥を閉じたのに、send 経路だけ取り残されている。攻撃者が被害者の `idem_key` を当てると `replay()` が被害者の記録をそのまま返す（他人の取引内容が読める）。
  修正方向は既に確立している（`charge_scope` と同じ形にする）が、既存レコードの移行が要るので schema 変更として扱うこと。

- **【MEDIUM】`server/moymoy-cs/src/merchant.rs:600` — 停止された加盟店をオーナー自身が即座に復帰できる。**
  `merchants.status` が行為者を区別しない単一カラムで、`set_status` のガードは `status != 'deleted'` だけ。運営向けの停止コマンドは `admin::run` に存在せず（`refund` のみ）DB 直接更新かオーナー経由になるが、どちらにせよオーナーが `POST /merchant/portal/status` に `active` を送れば戻せる。不正加盟店を止める操作を、その加盟店自身が取り消せる。
  **判断が要る点**: 運営による停止とオーナーによる停止を別の状態として持つか、運営専用の上書きフラグを足すか。認可モデルの変更。

- **【LOW】`server/moymoy-cs/src/payments.rs:475` — 加盟店 status の検査が資金移動の述語に入っていない。**
  `approve` は Argon2id 検証の前に一度 `is_active()` を見るだけで、claim UPDATE の述語は `state='created' AND expires_unix_ms > now` しか見ない。PIN 検証中（数百ミリ秒）に停止された加盟店が、進行中の承認から集金できる。

- **【MEDIUM】`mod/src/main/java/jp/houlab/mochidsuki/moymoy/MoyMoyExtension.java:234` — `emerald.charge` の重複排除が消費時点で耐久でもレース安全でもない。**
  `store.recorded(opId)` を connector の IO スレッド上で `server.submit(...)` の前に見るため、同じ `op_id` の並行到着で二重消費の窓がある。

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
- [ ] 本番設定: 任意 `MOYMOY_OTP_PEPPER` を運用者が env で設定（メール送信自体の bearer 設定は不要になった — `MOCHI_SVC_IDENTITY_TOKEN` は launcher が自動注入し、通常起動では常に有効）
- **チャージ成立の全チェーン**（`app → backend → Hub :7421 → mc-connector → mod → 消費 → ack → 着金`）。どれか欠けると在庫0/保留になる:
  - [x] **① 症状の可視化**: 在庫クエリの「応答なし/未発見/真の0」を潰さず区別（`character_unreachable`/`character_offline`＋両側ログ）`f3e8f40`。
  - [x] **② backend cert**: `deploy-backend.ps1 -EnableCharge` で mc-pki CA から `--mcserver-id moymoy` leaf 発行＋`MOCHI_MC_CERT_DIR` 設定。`run/app_backends/moymoy` に適用済（`can_charge=true`）。
  - [x] **③ Hub の :7421 有効化（真の根本原因）**: Hub が `MOCHI_HUB_MC_PKI_DIR` 未設定で「MC command bus (mTLS :7421) disabled」→ backend が connect timeout を繰り返し在庫0。`run/` の起動元 `MochiOS2.0/tools/win-hub-dev.ps1` に `MOCHI_HUB_MC_PKI_DIR=<repo>\.devstack\mc-pki\ca`＋`MOCHI_HUB_MC_PKI_FLAT=1` を追加（backend cert と同一 CA）。**Hub 再起動が必要**。
  - [ ] **④ MC サーバ側**: moymoy mod jar（`mod/build/libs/moymoy-*.jar`）＋ mochi connector mod を導入、`mochi-server.toml` の `mcserver_id` を非空に、mochi-mc-connector サイドカー稼働。※`hosted_app_ids` は**廃止**（自動広告）。
- [ ] backend 再配置（`deploy-backend.ps1 -EnableCharge` で moymoy-cs＋MC証明書を Hub workdir へ）。EC決済 Phase 2 完了により、フロントエンドが壊れる懸念は解消したので実施可能
- [ ] フル E2E（in-world で 0.2.2 再インストール → 口座開設(メール検証)→2FA→リカバリ→送金→チャージ の実機検証）
- [ ] 承認ゲート保留: `MOYMOY_OTP_PEPPER` の本番 fail-closed 化 / `AccountInfo` の email 型統合 / refresh 失敗の UI エラー状態化 / `run_inbound` 切断理由の可視化（mc-sdk 共有層）
- [x] **出金**（エメ→エメラルド）: backend（先引落→付与要求→ack確定、`AttestPurpose::Withdraw`、dead-letter方向別処理）／mod（`grants` 冪等ストア・二相コミット）／アプリ（チャージタブ内チャージ/出金セグメント＋ホームのクイックアクション）
- [x] 出金のフル E2E 実機検証（本番・online-mode サーバーで成功）。出金3件が `granted` = 要求額・返金0で `settled`（192 / 2,414 / 64 エメ、予約から決着まで 30〜50ms）。assertion の拒否は0件
- [ ] 出金の UX 再評価: 認証モーダルの成功表示は `ph-done` 700ms 固定（MochiOS `apps/com.mochi.ui/os-chrome.js`）。承認直後に閉じる不具合（エンベロープ読み違い）は MochiOS `eafd935d` で解消したので、本来の振り付けが出る状態で短すぎないかを実機で判断する
- [x] EC決済 Phase 1: backend 決済ドメイン（riskauth / merchant / payments、schema v6）
- [x] EC決済 Phase 2: MoyMoy アプリの承認オーバーレイ（`@owner_handle` 主表示・description の引用枠づけ・`from`/`launch_app_id` 不一致警告・新規加盟店バッジ・個人化シール）＋ 加盟店管理画面（登録・キー表示/rotate・停止・閉店・上限引き上げ）
- [x] EC決済 Phase 3: PiggleShop2 側の注文事前永続化・intent 作成→launch→サーバー照会確定への作り替え・**実機 E2E 成功**（本番の識別トークンで PiggleShop2 backend が moymoy-cs へ CONNECT、決済状態照会→配送まで到達を確認）
- [ ] リスクベース閾値（200 / 1,000 エメ）の実運用調整
- [ ] **将来実装**（着手前に承認要）: Web 決済（`return_url` 追加・MoyMoy ホスト型承認ページ、現行スキーマ/状態機械は無変更で使える）／ 返金 API（加盟店主導、加盟店口座から金が出る唯一の経路になるため着手時に別途承認）／ 署名レシート（moymoy-cs は `mochi-proto-attest` の `issue` feature を OFF にしており署名鍵を持たないため webhook 導入時に別途判断）／ 猶予付き二重 API キー（無停止ローテーション）／ webhook 通知
- [x] 入金通知（OS push、schema v7）: backend の transfer/credit 両プリミティブでの outbox 書き込み・配送タスク・端末リンク／MoyMoy アプリの登録配線／MochiOS 側の送信元認可再設計・v0.5.2 リリース公開（backend `5cb6342` / app `90a4fb0` / fix `c66df2a` / release `dfa624b`）
- [x] 入金通知の実機 E2E（送金 → 受取側端末の通知バナー到達を本番スタックで確認。初回失敗は Hub の staging コピー漏れ＝旧 allowlist が 404 を返したためで、機能側の欠陥ではなかった）
- [ ] **本番デプロイ**: エスクロー(schema v9〜v12)・step-up帯変更(v11)は実装・コミット済みだが本番未適用。PiggleShop2側のMCサーバー停止・mod入れ替えを伴うため、両リポジトリの同時デプロイ計画が必要（§デプロイの制約）。
