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
  - `mod/` — Forge 1.20.1 MC サーバーサイドmod → `moymoy.mc.mnn`。エメラルド消費/付与の真実。connector の `MnnServer` API に `MnnHandler` を実装(`MoyMoyExtension`)、呼び出し元は `MnnRequest#caller()` で `cs:app.moymoy` に固定（旧 `CommandDispatch.Handler` の名前突合は認可でないため廃止）。**オプショナル**。
- **エメラルドチャージ/出金**: チャージはアプリ起点＋ゲーム内 両対応、出金はアプリ起点のみ。双方向コマンドバス（backend が `cs_hosts:["moymoy"]` を claim、`reliable_send` 送信 / `run_inbound` 受信）。
- **整合性**: 「消費の真実=mod / 残高の権威=backend」を `emerald_ops` 台帳 + 二層冪等キー(`idem_key`/`op_id`) + at-least-once 再送 + 冪等決済で eventually-consistent に。
- **方針**: 旧 MoyMoy(`D:\IdeaProjects\MoyMoy`)はドメインの緩い参考のみ。MochiOS2.0 本体は原則無改変（app_backends 配置・mod jar 配置・`mcserver_id` 設定・証明書発行のみ。`hosted_app_ids` は廃止）。

---

## 現在の仕様（デザイン「MochiOS Mobile.html」駆動で確定）

デザインは 電子マネー×クレジットカード風のエメラルド決済アプリ。タブは **home / send(送る) / pay(支払う) / charge(チャージ、内部にチャージ/出金セグメント) / history(履歴)**。ボトムナビ5タブ・ホームのクイックアクション3つは不変。
通貨は整数「エメ」、**9エメ = 1エメラルドブロック**（Minecraft）。
取引種別 `kind`: `pay`(支払い) / `send`(送金) / `receive`(受取) / `charge`(チャージ) / `withdraw`(出金、引落は負・返金は正)。各取引 `{id, kind, label, amount(符号付), ts}`。
**請求/承認(request/approve)機能はデザインに無い** → 実装しない。

UIフロー:
- **home**: 利用可能残高 + カード(holder/number/expiry) + クイックアクション(pay/send/charge) + 最近の取引4件。
- **send**: フレンド(プレイヤー)選択 → 金額 → 確認 → 完了。残高減・相手は receive。
- **pay**: 近くの加盟店選択 → 金額 → 確認 → 完了、というデザイン当初の直接送金フローは v6 で廃止（`/wallet/pay` 削除、§EC決済）。決済は加盟店が発行する PaymentIntent の承認画面に置き換わった（実装済み、§EC決済）。
- **charge**: チャージ/出金セグメント切替。チャージはインベントリ(手持ちエメラルド + ブロック、9エメ=1ブロック)を換算 → 金額 → 確認 → 完了、エメラルド消費し残高加算。出金は金額 → 着金先キャラクター確認 → 完了、残高減で mod がエメラルド付与（§出金整合）。いずれも**MC mod 依存**。
- **history**: 全取引リスト(フィルタ: すべて/支払い/送金/チャージ/出金)。

### アカウントモデル（v2・独立アカウント + PIN）
**独立した MoyMoy アカウント（電子マネー型）**。`account_id` はサーバ生成 UUID で、Minecraft UUID とは独立。

- **資格情報**: `handle`（一意・小文字正規化・`[A-Za-z0-9_]` 3〜20）＋ `PIN`（4〜6桁数字, **Argon2id** ハッシュ保存）。handle は送金宛先（`@handle`）に兼用。
- **セッション**: register/login で 256bit ランダムトークンを発行し、HTTP ヘッダ `X-MoyMoy-Session` で送る。DB には **SHA-256 ハッシュ**で保存（期限 30日・logout で失効）。**backend が全ウォレットリクエストの本人を検証**（旧 mc_uuid 自己申告を解消）。
- **マルチアカウント**: 1端末に複数口座をリンク。クライアント保持リスト（`mochi.storage` / dev は localStorage）が正本で、ヘッダのアバターから切替・追加・ログアウト。サーバは `moymoy_sessions.phone_id` をメタデータ記録のみ。
- **MCキャラ連携（v5）**: 口座↔キャラの永続的な写像は保持しない。どのキャラのエメラルドを操作してよいかはリクエスト毎にユーザー同意付きの Hub 署名 attestation が決める（§出金整合「認可」参照）。`emerald_ops.attester_id` は同意されたサーバーへ再送を届けるためのルーティング情報であり、所有の証明ではない。
- **メール検証 / 2FA / リカバリ（v4）**: **MNN メール（`@*.mnn`）限定**。`MOCHI_MAIL_SERVICE_BEARER` 設定時は**開設にメール＋OTP必須**（1メール1口座、`email_lower` UNIQUE）、ログインは PIN＋メール2FA、PIN 忘れはメール OTP で再設定。未設定なら**従来の handle+PIN へ自動 degrade**。OTP は 6桁・SHA-256(+`MOYMOY_OTP_PEPPER`)保存・10分・5回上限・単回・再送クールダウン（`moymoy_otps`）。送信は `mochi-hub-mailer` の `MnnMailSender`（IPvM ゲートウェイ `/mail/otp-deliver` 経由で相手の in-world メールアプリへ配送。外部SMTPは使わない）。`valid_email` は `local@<単一ラベル>.mnn` のみ受理。dev 検証は `MOYMOY_DEV_OTP_LOG=1`（コードをログ出力）。

### バックエンド HTTP API
全レスポンス `{ok:bool, ...}`。ウォレット系は `X-MoyMoy-Session` でセッション認証（無効は 401）。
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
- `POST /wallet/_dev/credit {handle, amount}` → dev 専用クレジット（`MOYMOY_DEV_CREDIT=1` ゲート）
- **EC決済（MoyMoy Pay、v6）**: `GET /wallet/payment/intent?intent_id=`（承認画面が表示する唯一の情報源）／ `POST /wallet/payment/approve {intent_id, pin}`（常に PIN 必須）／ `POST /wallet/payment/decline {intent_id}`（PIN不要）。`POST /wallet/pay` は廃止（下記§EC決済）
- **加盟店 API（v6）**: `/merchant/v1/*`（`Authorization: Bearer moy_sk_…`、intent の作成・照会・取消）／ `/merchant/portal/*`（セッション+PIN、登録・キー再発行・停止・閉店・上限変更・一覧）。詳細は§EC決済

### SQLite スキーマ
`accounts`(account_id=MoyMoy口座UUID, handle, handle_lower, display_name, pin_hash, balance, holder, card_number, card_expiry, is_merchant, failed_pin_attempts, locked_until, **email, email_lower(UNIQUE), email_verified**) / `moymoy_sessions`(session_id, account_id, token_hash, phone_id, expires) / `moymoy_otps`(otp_id, purpose=signup|login2fa|recovery, email_lower, account_id, code_hash, payload_json, attempts, expires) / `transactions`(kind に `withdraw` 追加、引落は負・返金は正) / `merchants`（**v6拡張**: owner_account_id, name_skeleton(UTS#39 confusable skeleton の部分UNIQUE), status=active|disabled|deleted, api_key_hash/prefix/created/last_used, listed(既定0), payer_ref_salt, max_open_intents, daily_issue_cap） / `payment_intents`（**v6新規**: intent_id='pi_'+128bit CSPRNG, merchant_id, amount, description, order_ref, state=created|paid|declined|canceled|expired, payer_account_id, payer_hint_account_id, launch_app_id, tx_id, refunded_unix_ms, refund_tx_id, idem_key, expires_unix_ms） / `idempotency`(PK=idem_key,scope。出金は scope=`withdraw:<account_id>`、決済は scope=`mi:<merchant_id>` で加盟店ごとに分離) / `emerald_ops`(op_id, account_id=着金先, mc_uuid=消費キャラ, attester_id=同意されたサーバー(再送先を固定するルーティング情報。所有の証明ではない), direction=charge|withdraw, state=pending|sent|settled|failed|stuck, …)。出金は既存カラムに新しい値が増えるのみで**マイグレーション不要**（`kind`/`direction` に CHECK 制約なし）。マイグレーションは user_version ステッパ（v1 baseline `schema.sql` → v2 独立アカウント → v3 1キャラ1口座 → v4 メール/OTP → v5 `account_mc_links` を DROP しキャラクター所有の証明をリクエスト毎の Hub 署名 attestation へ移行 → v6 merchants 拡張 + payment_intents 新設 + デモ加盟店(m1..m5)を listed=0 に降格、各 `db/schema_vN.sql`）。詳細は `server/moymoy-cs/src/db/`。

### コマンドバス verb（backend→mod / mod→backend ack）
- `emerald.charge {op_id, idem_key, target_uuid, amount}` → mod 消費(インベントリのエメラルド+ブロック) → ack `{op_id, status, settled:consumed}`
- `emerald.withdraw {op_id, idem_key, target_uuid, amount}` → mod がエメラルド付与(`amount/9`ブロック+`amount%9`エメ、満杯時は足元ドロップ) → ack `{op_id, status, granted}`（`settled` とは別名。status: `ok`/`duplicate`(再付与せず同額を再ack)/`unknown`(claim済みだが付与を証明できず`stuck`・返金なし)/`player_offline`/`bad_request`/`unauthorized`/`internal_error`）
- `inventory.query {req_id, target_uuid}` → mod が手持ちを返答 `{req_id, emeralds, blocks}`（charge画面のインベントリ表示用。任意）

### 出金整合とattestation（§出金整合、チャージの逆方向）
出金 = エメ残高 → ゲーム内エメラルドで受け取る。チャージは「先に消費 → ack で入金」、出金は「先に引落(予約) → 付与要求 → ack で確定」と安全な失敗の向きが逆転する（付与を先にするとエメラルドが無から増えインフレする）。
- 確定的失敗（`player_offline` 等・付与なし）は同一トランザクション内で即返金。付与されたか**不明**（`unknown`）な場合は返金しない（返金するとエメラルドと残高の両取りになる）ため `stuck` にして手動レビュー（R008 と同じ規律）。返金は state を非終端から動かした当のトランザクション内でのみ行い、UPDATE の変更行数でガードして reconcile と ack の同時到達による二重返金を構造的に防ぐ。
- `reconcile` の dead-letter は方向別: チャージの一括 UPDATE には `direction='charge'` の絞りが入り、出金は1行ずつ処理（未送達 `pending` → 返金して `failed` ／曖昧な `sent` → 返金なしで `stuck`）。再送は台帳の `direction` で verb を分岐（出金 op をチャージとして再送するとプレイヤーのエメラルドを逆に没収するため必須）。
- **認可**: `AttestPurpose::Withdraw`（purpose 文字列 `"withdraw"`）と request-hash ドメイン `moymoy.withdraw.v1` を新設。ドメイン分離＋challenge の purpose バインドにより、チャージ用 assertion で出金はできず逆も不可。
- **上限**: 1操作あたり 20,736 エメ（=2,304 エメラルドブロック=インベントリ1個分）を backend と mod が独立に強制（無制限だと mod 側で数百万スタックが生成されサーバースレッドを固めるため）。
- **mod 側冪等**: `grants` コンパウンド（`op_id → -1`=claim済み未確定／`≥0`=確定付与額）で管理。チャージ用の `ops` は不変・後方互換。claim → ディスクへ同期フラッシュ → 付与 → 確定記録 → フラッシュ、の二相（付与は無から生成するため記録前クラッシュ＋リプレイでの二重生成を防ぐ必須要件）。判定〜claim〜付与〜確定は1回の `server.submit` 内で完結させ、同一 op の同時実行が両方とも「未 claim」を観測することを構造的に不可能にしている。
- **不変条件の変更**: 従来「ウォレットから価値が外へ出る経路は無い」が成立していたが、出金ではこれは成立しない。代わりに ①出金は必ずセッション本人の操作、②金額と `idem_key` に束縛されたユーザー同意付き assertion を要する、③`AttestedFacts::account_id` は依然として認可に読まれない、④attester（サーバー運営者）は他人の残高を動かす手段を持たず宛先キャラを名乗れるのみ、が担保される。新たな信頼境界は「ユーザーが出金先 MC サーバーを信用する」こと。

### EC決済（MoyMoy Pay、v6）
MochiOS 内の EC サイトが MoyMoy で決済する仕組み。VISA 3D Secure と同型のリダイレクトフロー。**第三者開発者も使う公開 API**（第一号加盟店 PiggleShop2）。**第三者開発者向けの入口は `docs/merchant-quickstart.md`**（登録→キー→intent作成→起動→照会確定の最短ルート）。

フロー: EC バックエンドが API キーで PaymentIntent を作成 → EC アプリが `os.apps.launch("com.mochi.moymoy", {intent_id})` で MoyMoy を起動（渡すのは `intent_id` のみ）→ MoyMoy が自バックエンドから intent 詳細を取得（`GET /wallet/payment/intent`）して承認画面を表示 → PIN → `POST /wallet/payment/approve` が同一トランザクションで intent claim + 送金 → EC へ戻る → **EC バックエンドが moymoy-cs へ照会して `paid` を確認してから履行**（クライアントの「払った」申告は信用しない）。

不変条件（出金で確立したものを移植）: ①資金移動は必ずセッション本人の操作で、**API キーではいかなる残高も動かせない**（intent の作成・照会・取消のみ） ②承認画面が表示する金額・加盟店名は moymoy-cs の記録が唯一の真実で、クライアント渡しで信用するのは `intent_id` のみ ③第三者は「宛先」を名乗れるだけで「誰の財布が払うか」は名乗れない ④資金移動は `state='created' AND expires_unix_ms > now` を条件とした確定的 UPDATE 一文（変更行数==1のときのみ transfer へ進む）。この `now` は**確定処理の内部で読む**（承認ハンドラの入口から受け取らない） — 到着時刻を使うと Argon2id 比較やロック待ちで数百ms 空くため、PIN 検証中に期限が切れても送金が成立してしまう。

intent の状態機械: `created → paid / declined / canceled / expired`。全終端は最終（`paid` はリファンドでも巻き戻らず、逆方向の別トランザクションになる）。二重承認・期限切れ直前承認・加盟店キャンセルとの競合は上記 UPDATE 一文で排他される。承認前チェックとして `merchants.status='active'` を必須、`payer_hint_account_id` 指定時は他口座からの承認/拒否を `payer_mismatch` で拒否、リプレイ（`paid` かつ同一 payer）は PIN 検証より前に応答して試行回数を消費しない。

加盟店: セルフサーブ登録。1口座あたり保有できる加盟店は `MAX_MERCHANTS_PER_ACCOUNT`（3件、`status != 'deleted'` を数える。停止(disabled)中も枠を占有し続ける — 名前とスロットは同じ予算で、`close` 以外に返す手段が無いため）。API キーは `moy_sk_` + 256bit CSPRNG、DB には SHA-256 ハッシュのみ保存、平文は登録/rotate 応答で一度だけ返す。`/merchant/v1/*`（API キー認証、intent の作成・照会・取消。移動可能な金額はゼロ）と `/merchant/portal/*`（セッション+PIN、登録・キー再発行・停止・閉店・上限変更）の2系統分離。加盟店名は UTS #39 confusable skeleton で一意化（`lower`+NFKC だけでは別スクリプトの同形異字 `PiggleShoр2`(Cyrillic er) が素通りする）し、スクリプト混在を拒否、運営語彙（`moymoy`/`公式`等）を予約語として禁止。**リネーム API は存在しない**（登録時固定）。`description`/`sub`/`name` は NFKC 後 Cc+Cf（bidi制御・ZWJ含む）+未割当+私用領域を拒否し結合文字を基底1文字あたり3個までに制限（`U+202E` 等での承認画面偽装を防ぐ）。**登録のレート制限は2段構え**: 実際に店が作られた（＝有効な名前・PIN・空き枠がすべて揃った）ときだけ10分に1回の枠を消費する（名前の打ち間違い等の失敗では消費しない）。別に、試行そのもの（成否問わず）を絞る5回/10秒のバースト枠があり、これはセッションを握った攻撃者が無効な名前を連打してPIN照合のCPUを消費させる手口を防ぐためのもの。

**閉店（`POST /merchant/portal/close`、セッション+PIN）**: `status='deleted'` の soft delete（`name_skeleton`/`api_key_hash`/`api_key_prefix` を NULL・`listed=0`）で、名前・資格情報・枠を解放しつつ台帳（`payment_intents.merchant_id` の参照先）は残す。行ごと削除すると「一度でも取引した店は閉じられない」か「注文履歴を道連れにする」のどちらかになるため。**未決済 intent が1件でもあれば拒否される**（`open_intents`、count 付き）。閉じた加盟店は再オープン不可（登録し直す）。

**加盟店の発行上限**（下記「加盟店売上を拘束しない」ことの代償措置）: 未決済 intent 件数上限（既定20、上限500）と 24h 発行合計金額上限（既定50,000エメ、上限2,000,000エメ）。引き上げは `/merchant/portal/limits`（セッション+PIN）のみ、API キー単体では不可。

**運営の強制返金**: `moymoy-cs admin refund <intent_id> [reason...]`（CLI サブコマンド）で実装済み。`paid` は巻き戻さず、逆方向の別トランザクションとして記録する。**HTTP エンドポイントには出さない** — 他人の口座から同意なく金を動かす最も強い権限操作なので、盗まれたセッション・漏れた API キー・ルーティングのバグから守る手段は、そもそもネットワーク面を作らないことにした。

**シェルの attestation モーダルは決済承認に使わない**（決定）。MochiOS の `docs/developers/host-attestation.md` §3 が明示的に禁止しており、`reason`（ユーザーに見える文言）は署名クレームに入らず `request_hash`（署名される値）はユーザーに見えないため、見たものと署名したものが構造的に一致しない。承認は MoyMoy アプリ内の PIN が担う。※MochiOS 側ソースは未照合（moymoy-cs リポジトリからは検証不能）。

OS 依存（決定、未照合）: MochiOS の `os.apps.launch` / `os.apps.takeLaunchIntent`（Phase 0 実装済とされる）。ウォームスイッチではクエリが URL に載らず失われるため、`intent_id` はホスト側メールボックス経由で渡す。

### リスクベース段階認証（riskauth、v6）
機能別に PIN を撒かず、資金流出の唯一の関門にする。`/wallet/send`・`/wallet/withdraw`・決済承認 (`/wallet/payment/approve`) が同じ関門 (`riskauth::step_up`) を通る。`/wallet/charge` は資金流入なので対象外。

3段階の閾値（コード定数、環境変数化はしない。実運用で調整する前提）:
- 単発 **200 エメ以下**かつ 24h 累計流出 **1,000 エメ以下**かつ端末一致 → **認証なし**
- それ以外 → **PIN**
- 単発 **5,000 エメ超**、または 24h 累計流出 **10,000 エメ超**、または**端末不一致** → **PIN + メール OTP**

決済は既定で常に PIN 以上（`Requirement::Pin` が floor で、金額・端末不一致はそれをさらに引き上げることしかできない）。端末一致は `moymoy_sessions.phone_id`（account の最古の phone_id 保持セッションとの照合、一度も device id を送っていないアカウントは既存ユーザー保護のため常に「一致」扱い）。**この列は v6 以前は保存されるだけで一度も照合されていなかった。**

メール OTP を要求する第三段階で、口座にメール未検証（または deploy 側でメール未設定）の場合は **PIN だけで通さず拒否する**（`otp_unavailable`）。閾値が「決めない」状態を作らないための fail-closed。

**OTP の検証は送金・決済の確定トランザクションとは別のトランザクションで、両方の結果で必ず commit する。** 畳んで確定処理の中で検証すると、コード誤り時にロールバックで OTP の失敗カウンタごと巻き戻ってしまい、6桁コードへの総当りに5回の上限が掛からなくなる（一度実際に起きた不具合）。正しい PIN・誤った OTP の場合は PIN の失敗カウンタだけ払い戻す（OTP を打ち間違えただけで口座がロックされないように）。この分離は「後で統合した方が綺麗」に見えて壊れる箇所なので、理由ごと維持する。

PIN 検証は「短いトランザクションで失敗カウンタを先に書いて commit → トランザクション外で Argon2id → 短いトランザクションでロックアウト再検証しカウンタをクリア」の3段構え。単一トランザクション内で Argon2id を回すと SQLite の単一ライタロックを数百ミリ秒保持し、ウォレット全体が停止するため。PIN が正しかったが操作自体が成立しなかった場合（残高不足等）は `refund_attempt` で消費した試行を返却する（さもないと残高不足への正しい PIN リトライ5回で自分の口座をロックする）。

`/wallet/pay`（デモ加盟店ハードコードの直接送金経路）は PaymentIntent が置き換えたため**削除済み**。デモ加盟店(m1..m5)は全件 `listed=0` に降格し、`GET /wallet/merchants` は `listed=1 AND status='active'` のみを返す。

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
- **決済の信頼境界（同じ性質のリスク）**: 加盟店売上を拘束しない設計決定（下記）により、詐欺加盟店が集金直後に `/wallet/withdraw` で MC 世界へ抜けた場合、強制返金の原資が残らず回収不能になる。出金機能で「出金先 MC サーバーを信用する」を受容したのと同じ性質の、同意の上で受け入れるリスク。代償措置として加盟店ごとの未決済 intent 件数上限・24h 発行合計金額上限で集金速度を制限（§EC決済）。
- **`LockoutPolicy::Bypass`（加盟店の緊急停止）の増幅**: `/merchant/portal/status` で shop を disabled にする操作は口座ロックアウト中でも PIN を試させるが、`begin_pin_attempt` は Bypass でも `locked_until` への書き込み自体は行う（拒否判定だけスキップする）。セッションを奪った攻撃者が被害者の他エンドポイントを誤 PIN で継続的にロックできる増幅がある。auth の意味論変更になるため未対処。
- `riskauth::outflow_24h` は `transactions` の負の amount を全て合算するため、返金済みの出金も 24h 枠を消費し続ける（摩擦が増える安全側の歪み）。
- PIN のロックアウトカウンタ（`accounts.failed_pin_attempts`、5回で15分ロック）はログインと共有のため、handle を知る第三者が誤 PIN 5回で被害者の送金/出金/決済を15分単位で止められる。決済・送金・出金側はセッション単位の指数バックオフ（`PinBackoff`）を併用して緩和している。
- 加盟店の `launch_app_id` は intent 作成時の自己申告で、その app_id が本当にその加盟店のものかを moymoy-cs は検証できない。承認画面での OS 由来 `from` との不一致警告は実装済みだが、これは表示レベルの注意喚起であり、`launch_app_id` の真正性を moymoy-cs 側で検証する仕組みではない。
- **CodeX 再レビュー（反映済）**: v2 再設計に recursive-codex-reviewer を実施。妥当指摘を反映 — backend `382acc2`（冪等の複合PK化で二重決済防止 / `user_version` を tx 内へ移しマイグレーション原子化 / 握り潰しログ化 ほか）、frontend `ffb40c8`（`me()` を ok/expired/unknown で識別し一時エラーで口座を消さない / アンマウントガード / 401 即時処理 ほか）。
- **承認ゲート保留（共有層に跨る設計課題・未着手）**: 着手前に設計案の承認が必要。
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
