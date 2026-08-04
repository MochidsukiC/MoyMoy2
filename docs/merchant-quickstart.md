# MoyMoy Pay 加盟店クイックスタート

MoyMoy を自分の EC アプリの決済手段として組み込むための最短ルート。**VISA 3D Secure と同型**のリダイレクト決済で、加盟店の API キーではいかなる残高も動かせない。

## 1. フロー

```
[EC バックエンド] ──① intent 作成 (API キー)──────────▶ [moymoy-cs]
      │                                                     ▲
      │ ② os.apps.launch("com.mochi.moymoy", {intent_id})   │
[EC アプリ] ─────────────────────────────────▶ [MoyMoy アプリ]
                                                    │ ③ 承認画面（表示は moymoy-cs の記録のみ）→ PIN
[EC アプリへ復帰] ◀─────────────────────────────────┘
      │
      └─④ EC バックエンドが moymoy-cs へ照会 → `paid` を確認してから履行
```

**不変条件**: API キーは intent の作成・照会・取消しかできず、残高は一切動かせない。資金移動は必ず支払者本人のセッション + PIN でのみ起こる。承認画面が表示する金額・加盟店名は moymoy-cs の記録が唯一の真実で、EC アプリがクライアントに渡すのは `intent_id` だけ。**だから④の照会を省略してはいけない** — クライアントの「払いました」という申告には何の根拠もない。

## 2. 前提

- 加盟店になるには MoyMoy の口座（handle + PIN）が要る。**売上は登録者本人の口座に入る**。
- 金額は**整数エメ**のみ（小数不可）。丸め方は §8。
- moymoy-cs は `https://moymoy.cs.mnn` の e2e TLS backend。到達方法は §4。

## 3. 加盟店登録と API キー

### 3-0. MoyMoy 口座でログインする

登録・キー管理はセッション + PIN が要る。まだアカウントが無ければ `/auth/register` で作成し、あれば `/auth/login` でセッションを取る。

```bash
curl -X POST https://moymoy.cs.mnn/auth/login \
  -H 'content-type: application/json' \
  -d '{"handle":"myshop_owner","pin":"1234"}'
# → {"ok":true,"session":"<token>","account":{...}}
```

以降のセッション認証エンドポイントは全て `X-MoyMoy-Session: <token>` ヘッダで呼ぶ。

### 3-1. 加盟店を登録する

```bash
curl -X POST https://moymoy.cs.mnn/merchant/portal/register \
  -H 'content-type: application/json' \
  -H 'X-MoyMoy-Session: <token>' \
  -d '{"name":"Piggle Shop","sub":"雑貨","pin":"1234"}'
# → {"ok":true,"merchant_id":"mr_...","name":"Piggle Shop","api_key":"moy_sk_...","api_key_prefix":"moy_sk_ab12cd"}
```

**`api_key` は応答に一度だけ出る。DB にはハッシュしか残らないので、取り逃すと再表示できない。** 環境変数などに置き、コードに埋め込まない。

制約:
- 名前は登録後**変更不可**（リネーム API は無い。承認画面表示中に名前が変わる余地を作らないため）。
- 名前は同形異字（例: `PiggleShoр2` のキリル文字 `р`）とスクリプト混在を拒否し、`moymoy`/`公式`等の運営語彙は予約語として拒否される。
- 1口座あたり保有できる加盟店は最大3件（閉じた分を除く）。

登録失敗時のエラー: `bad_name`（`reason`: `empty`/`too_long`/`invisible_char`/`stacked_marks`/`reserved_name`/`mixed_script`/`unnameable`）、`bad_sub`（同様）、`name_taken`、`too_many_merchants`。

### 3-2. キーのローテーション・停止

```bash
# 旧キーを即時無効化して新キーを発行
curl -X POST https://moymoy.cs.mnn/merchant/portal/key \
  -H 'content-type: application/json' -H 'X-MoyMoy-Session: <token>' \
  -d '{"merchant_id":"mr_...","pin":"1234"}'

# 緊急停止（intent の作成・承認を即座に止める）
curl -X POST https://moymoy.cs.mnn/merchant/portal/status \
  -H 'content-type: application/json' -H 'X-MoyMoy-Session: <token>' \
  -d '{"merchant_id":"mr_...","status":"disabled","pin":"1234"}'
```

キーのローテーションはセッション + PIN が要る。**API キーが漏れても、加盟店設定の乗っ取りやキー単体での再発行はできない。**

> アプリ内の加盟店管理画面（キー表示/rotate・停止・上限引き上げの UI）は開発中（Phase 2）。現時点では上記のように API を直接叩く。

## 4. 到達方法（ここでつまずきやすい）

moymoy-cs は e2e TLS backend なので、**CONNECT トンネル経由でしか到達できない**。cs backend 同士が使う `CsHttpSender`（reverse tunnel の origin-form）は plaintext HTTP 限定で、TLS backend には使えない。

```rust
// Cargo.toml: reqwest = { features = ["rustls-tls-manual-roots"] }
let anchor_pem = std::fs::read(
    std::path::Path::new(&std::env::var("MOCHI_STATE_DIR")?).join("mnn-tls-anchor.pem"),
)?;
let gateway = std::env::var("MOCHI_IPVM_GATEWAY").unwrap_or_else(|_| "127.0.0.1:7411".into());
let bearer = std::env::var("MOCHI_SVC_IDENTITY_TOKEN")?;

let mut proxy = reqwest::Proxy::all(format!("http://{gateway}"))?;
let auth_header: reqwest::header::HeaderValue = format!("Bearer {bearer}").parse()?;
proxy = proxy.custom_http_auth(auth_header); // Proxy-Authorization、Authorization ではない

let mut builder = reqwest::Client::builder().proxy(proxy);
for cert in reqwest::Certificate::from_pem_bundle(&anchor_pem)? {
    builder = builder.add_root_certificate(cert);
}
let client = builder.build()?;

client.get("https://moymoy.cs.mnn/healthz").send().await?;
```

新しい環境変数は不要（`MOCHI_STATE_DIR` / `MOCHI_IPVM_GATEWAY` / `MOCHI_SVC_IDENTITY_TOKEN` は launcher が起動時に注入する）。

> **注意（取り違えると危険）**: root cert に追加してよいのは `mnn-tls-anchor.pem` だけ。同じ state dir にある `mnn-trust-anchor.pem` は別物（制約のない Overlay CA）で、これを TLS の信頼ストアに入れると無制約の CA を信頼することになる。
>
> `danger_accept_invalid_certs(true)` は使わない。証明書検証を切って「繋がった」ことは、加盟店 API が要求する検証済みチャネルの代わりにならない。

到達性を素早く確認したいときは `server/piggleshop-cs/examples/moymoy_reach_spike.rs`（`cargo run --example moymoy_reach_spike`）が動くサンプル。本番トークン（`MOCHI_SVC_IDENTITY_TOKEN`）での CONNECT は開発トークン（`AllowAllAuthClient`）ほど広く実証されていないので、初回導入時は必ずこの spike で自分の環境の到達性を確認すること。

## 5. intent を作る

```bash
curl -X POST https://moymoy.cs.mnn/merchant/v1/intent/create \
  -H 'content-type: application/json' \
  -H 'Authorization: Bearer moy_sk_...' \
  -d '{
    "idem_key": "order-abc123",
    "amount": 16,
    "description": "りんご 1個",
    "order_ref": "abc123",
    "expires_in_secs": 600
  }'
# → {"ok":true,"intent_id":"pi_...","state":"created","amount":16,"expires_unix_ms":...}
```

フィールド:
- `idem_key`: **自分の注文 ID を使う**。同じ `idem_key` での再送は新しい intent を作らず、最初の応答をそのまま再生する（自分の加盟店の名前空間内でのみ有効なので、他加盟店の `idem_key` とは衝突しない）。
- `amount`: 整数エメ、1 以上 `MAX_AMOUNT`(10億) 以下。
- `description`: 加盟店が入力した文言として承認画面に引用枠で表示される。制御文字・bidi制御・未割当は拒否される。
- `expires_in_secs`: 省略時 600 秒。60〜1800 秒の範囲。短すぎると PIN 入力が間に合わず、長すぎると閉店後の加盟店に対する承認画面が生き続ける。
- `launch_app_id`（任意）: 決済を開始したアプリの `app_id` の自己申告。MoyMoy は OS 由来の起動元と突き合わせて不一致なら警告バナーを出す（ブロックはしない）。
- `payer_hint_handle`（任意）: 支払者を `@handle` で指定する。指定すると他の口座からの承認・拒否は `payer_mismatch` で拒否される（支払者を「縛る」のであって「代わりに払わせる」のではない）。

加盟店ごとの発行上限（拘束されない売上への代償措置）: 未決済 intent 件数の上限（既定20件）と 24h 発行合計金額の上限（既定50,000エメ）。超えると `too_many_open_intents` / `daily_issue_cap` で拒否される。引き上げは `/merchant/portal/limits`（セッション + PIN）のみ。

## 6. MoyMoy を起動する

MochiOS の launch-intent 機構で、`intent_id` だけを渡して MoyMoy を起動する。

```js
mochi.os.apps.launch("com.mochi.moymoy", { intent_id: "pi_..." });
```

自分の manifest に `apps.launch` の宣言（`ServiceScope`）が必要。**MoyMoy は `params` から `intent_id` だけをホワイトリスト抽出し、他のキーは読まない** — 金額や加盟店名を一緒に渡しても無視される。表示は必ず moymoy-cs 自身の記録から取る。

## 7. 結果を受け取る（最重要）

EC アプリが復帰したら、**自分のバックエンド経由で** moymoy-cs に照会し、`paid` を確認してから履行する。

```bash
curl https://moymoy.cs.mnn/merchant/v1/intent?intent_id=pi_... \
  -H 'Authorization: Bearer moy_sk_...'
# → {"ok":true,"intent":{"intent_id":"pi_...","state":"paid","amount":16,"payer_ref":"...","refunded":false,...}}
```

**クライアントの「払いました」という申告を信じてはならない。** 改造されたクライアントは UI 上どんな状態でも表示できるので、EC アプリ側の判断には何の意味も無い。信じてよいのはこの照会の応答だけ。

ポーリングは数秒間隔で、`expires_unix_ms` を過ぎたら打ち切って `expired` として扱ってよい（サーバー側も同じ猶予で `state: "expired"` へ遷移する）。

`payer_ref` は加盟店ごとに派生する不可逆な安定 ID。同一支払者は同一加盟店内で常に同じ値になるので再犯検知に使えるが、加盟店を跨いでは相関できない（実 handle は漏れない）。

## 8. 金額の扱い

MoyMoy は整数エメしか受け付けない。小数価格を持つカタログを整数エメに変換するのは加盟店側の責任。**PiggleShop の実装が worked example**（`server/piggleshop-cs/src/orders.rs`。金額計算ロジックは実装・テスト済みだが、moymoy-cs への intent 作成呼び出しはまだ配線されていない ── §11 参照）:

1. カタログ価格を **1/100 エメ単位の整数**（"cents"）に変換してから足し算する。浮動小数のまま `0.05 + 0.05 + 0.05` を足すと `0.15000000000000002` のような誤差が乗る。
2. 丸めはカート**合計に一度だけ**適用し、**切り上げる**。行ごとに丸めると誤差が複利的に膨らむ（36行の 0.05 エメ商品を行ごとに切り上げると 36 エメ請求になりかねないところを、合計一度なら 4 エメで済む）。切り捨てや四捨五入は、安い商品だけの小さなカートを 0 エメで確定させてしまう危険がある。

```rust
// cents は i64、価格計算の唯一の float→integer 境界
let cents = (price * 100.0).round(); // truncate ではなく round — 4.25 が 4.2499999… で来ても 425 になる
// … 全行を cents で合算 …
let eme = (total_cents + 99) / 100; // 合計に一度だけ、切り上げ
```

## 9. エラーとリトライ

| コード | 意味 | 対処 |
|---|---|---|
| `bad_amount` | 金額が 1 未満 または `MAX_AMOUNT`(10億) 超 | 金額を直す |
| `bad_description` / `bad_order_ref` | 不正な文字列（`reason` 参照） | 制御文字・bidi制御・連続する結合文字を除く |
| `bad_expires_in_secs` | TTL が 60〜1800 秒の範囲外 | 範囲内の値に直す（`min`/`max` が応答に入る） |
| `too_many_open_intents` | 未決済 intent が上限（既定20）超過 | 古い intent を `cancel` するか、上限を portal で引き上げる |
| `daily_issue_cap` | 24h 発行合計が上限（既定50,000エメ）超過 | 翌日を待つか上限を引き上げる |
| `unknown_payer_hint` | `payer_hint_handle` が存在しない handle | handle を確認する |
| `unknown_intent` | 存在しない、または他加盟店の intent_id | `intent_id` を確認する |
| `already_paid` | 既に `paid`（cancel でも履行義務は消えない） | 履行済みか確認し、二重発送しない |
| `not_cancelable` | `paid`/`canceled`/`expired` を再度 cancel した | 現在の `state` を見て判断 |
| 429 `rate_limited` | 呼び出し過多（作成30/分・照会120/分・登録は口座あたり1回/10分） | `retry_after_ms` 待って再試行 |
| 401 `invalid merchant API key` | キーが誤り・失効済み | ローテーション済みでないか確認 |
| 403 `merchant is disabled` | 加盟店が停止中 | portal で `active` に戻す |

`idem_key` は再送に対して安全 ── 同じキーでの再作成は新しい intent を作らず最初の応答を返す。ネットワークエラー時はそのまま同じ `idem_key` でリトライしてよい。

## 10. セキュリティ チェックリスト

- [ ] `api_key` は環境変数などに保管し、リポジトリやクライアントコードに埋め込まない。
- [ ] 金額は**必ずサーバー側で計算**する。クライアントが送ってきた金額を intent に使わない。
- [ ] 注文は**決済より先に永続化**する（`awaiting_payment` 相当の行を先に書く）。決済の瞬間に何を払っているかの記録が無いと、応答が失われたときに突合できない。
- [ ] 復帰後は必ず**サーバー間の照会**で `paid` を確認してから履行する。クライアント申告を信じない。
- [ ] キー漏洩時はまず `disabled` にして被害を止め、ローテーションして再開する（ローテーションはセッション + PIN が要るので、キー単体を盗んだ攻撃者はローテーションもできない）。
- [ ] `mnn-trust-anchor.pem` を TLS の信頼ストアに入れない（§4）。`danger_accept_invalid_certs` を使わない。

## 11. 現時点で無いもの

- **webhook は無い。** 結果はポーリングで取得する。
- **加盟店主導の返金 API は無い。** 運営による強制返金のみ（CLI 経由、加盟店 API からは呼べない）。
- **署名レシートは無い。** 照会は API キー + TLS の認証済みチャネルの応答をそのまま信頼できる前提。
- **アプリ内の加盟店管理画面は開発中（Phase 2）。** 現時点では portal API を直接叩く。
- PiggleShop の `orders.rs` にある金額計算・状態機械は実装・テスト済みだが、**moymoy-cs への intent 作成呼び出しの配線自体は未実装**（Phase 3 待ち）。worked example として参照する際は計算ロジックのみを見ること。

受容しているリスクや設計判断の背景（加盟店売上を拘束しない理由、第二要素を持たない理由など）は `DEV.md` を参照。
