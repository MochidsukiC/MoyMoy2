# MoyMoy Pay 加盟店クイックスタート

MoyMoy を自分の EC アプリの決済手段として組み込むための最短ルート。**VISA 3D Secure と同型**のリダイレクト決済で、加盟店の API キーではいかなる残高も動かせない。

## 1. フロー

```
[EC バックエンド] ──① intent 作成 (API キー)──────────▶ [moymoy-cs]
      │                                                     ▲
      │ ② os.apps.launch("com.mochi.moymoy", {intent_id})   │
      │    （`.mnn` Web ページからの起動は {intent_id, return_url}。§6）
[EC アプリ] ─────────────────────────────────▶ [MoyMoy アプリ]
                                                    │ ③ 承認画面（表示は moymoy-cs の記録のみ）→ PIN
[EC アプリへ復帰] ◀─────────────────────────────────┘
      │
      └─④ EC バックエンドが moymoy-cs へ照会 → `paid` を確認してから履行
```

**不変条件**: API キーは intent の作成・照会・取消しかできず、残高は一切動かせない。資金移動は必ず支払者本人のセッション + PIN でのみ起こる。承認画面が表示する金額・加盟店名は moymoy-cs の記録が唯一の真実で、EC アプリがクライアントに渡すのは `intent_id` だけ。**だから④の照会を省略してはいけない** — クライアントの「払いました」という申告には何の根拠もない。

## 2. 前提

- 加盟店になるには MoyMoy の口座（handle + PIN）が要る。**売上は登録者本人の口座に入る**。
- 金額は **1/100 エメの整数マイナーユニット**で送る（フィールド名は `amount_minor`。`1600` が 16 エメ）。§5・§8。
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

`api_key_prefix` は `moy_sk_` という接頭辞そのものではなく、**平文キーの先頭13文字**（`moy_sk_` + 本体6文字）。手元のどのキーが登録済みかを照合するなら `key.startsWith(api_key_prefix)`。閉店した加盟店では NULL になる。

制約:
- 名前は登録後**変更不可**（リネーム API は無い。承認画面表示中に名前が変わる余地を作らないため）。
- 名前は同形異字（例: `PiggleShoр2` のキリル文字 `р`）とスクリプト混在を拒否し、`moymoy`/`公式`等の運営語彙は予約語として拒否される。
- **保存され返ってくる店名は NFKC 正規化 + trim 後の文字列**（`㈱` は `(株)` に変わり、文字数も 1 から 3 に増える）。送った文字列がそのまま残る前提を置かず、表示には応答の `name` を使う。
- 1口座あたり保有できる加盟店は最大3件で、**停止中（`disabled`）も枠を数える**。枠を返せるのは閉店（`/merchant/portal/close`）だけで、その閉店も未決済 intent が1件でもあれば `open_intents`、エスクロー中の資金が残っていれば `escrowed_funds` で拒否される。**閉じた加盟店は再オープンできない**（登録し直す）。
- **登録に成功すると10分間、同じ口座からの次の登録が 429 になる。3件を続けて登録することはできない。** 名前の打ち間違い・PIN 違い・枠超過のような失敗ではこの10分枠を消費しないので即座に打ち直せる（ただし試行そのものには5回/10秒のバースト枠がある）。

登録失敗時のエラー:
- `bad_name`（`reason`: `empty`/`too_long`/`invisible_char`/`stacked_marks`/`reserved_name`/`mixed_script`/`unnameable`）、`bad_sub`（同様）、`name_taken`。
- `too_many_merchants` — 応答に `limit` が入るので、3 という値を加盟店側に持たなくてよい。
- `email_verification_required` — **HTTP 200** + `{"ok":false,"error":"email_verification_required"}`。フィールドではなくエラーコードで返る。条件は「サーバの email 機能が有効」かつ「その口座のメールが未検証」の AND で、`GET /auth/config` の `email_enabled` を見れば前者は事前に判別できる。判定は**PIN 照合の後**なので、PIN が違えば `invalid_pin` が先に返る。
- `invalid_pin` / `locked` / `too_many_attempts` — いずれも HTTP 200 で、`retry_after_ms` が付く。
- `rate_limited` — **HTTP 429** + `retry_after_ms`。**`Retry-After` ヘッダは付かない**ので、待ち時間は本文から読む。

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

> アプリ内にも加盟店管理画面がある（キー接頭辞と最終利用時刻の表示・キー再発行・停止/再開・発行上限の変更・売上ページ）。ボトムナビのタブからは入れず、到達手順は**ウォレット画面ヘッダ右上のアバター → シート「アカウント」の「設定」→「加盟店管理（MoyMoy で代金を受け取る）」**。登録もここから行える（一覧下部の「＋ 加盟店を登録する」→「PIN で確認して登録」→ PIN 画面の「登録する」）。アプリ自身の表記法に揃えるなら「設定 › 加盟店管理」。閉店（`/merchant/portal/close`）の UI は無いので、閉店だけは API を直接叩く。

### 3-3. 応答の読み方と副作用

- **業務的な失敗はほぼ全て HTTP 200 + `{"ok":false,"error":...}` で返る。** 非 200 は 401 / 403 / 429 / 500 と、リクエストがそもそも読めなかった 400 / 415 / 422 だけ。**ステータスコードで成否を判定する実装は誤動作する** — 見るのは `ok`。
- ただし **400 / 415 / 422 の本文は JSON ではない**（axum の抽出器が返す text/plain で、`ok` フィールドを持たない）。415 は `content-type: application/json` の付け忘れ、422 は JSON としては読めるが型が合わない場合（必須フィールドの欠落、`amount_minor` に `1600.0` のような float や文字列を入れた場合）。**`amount_minor` の型を間違えると `unsupported_amount_unit` ではなく 422 になる** — serde のデータエラーの方が先に出るため。本文を無条件に `JSON.parse` するクライアントはここで例外を投げる。
- **401 と 403 の境界**: 「停止中（`disabled`）」は 403 `merchant is disabled`、「閉店（`deleted`）」は **401** `invalid merchant API key`。閉店が `api_key_hash` を NULL にするため、閉店した加盟店のキーは無効なキーと区別が付かなくなる。
- `GET /merchant/v1/intent` は**読み取りに見えて副作用がある**。キーが提示されるたび `api_key_last_used_unix_ms` を更新し（加盟店管理画面に「キー最終利用」として出る、**鍵の漏洩を検知するための信号**）、読み取りレート枠を1消費する（120回/60秒）。鍵の疎通確認に使うのは構わないが、注文と無関係に定期ポーリングすると最終利用時刻が常に「たった今」になり、漏洩検知が潰れる（決済を待つ間のポーリングは §7 のとおりで、これは注文の分だけで止まる）。
- 存在しない `intent_id` も他店の `intent_id` も、どちらも 200 `unknown_intent` で返り区別が付かない。他店の注文フローを覗く手段にしないための意図的な設計。

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
    "amount_minor": 1600,
    "description": "りんご 1個",
    "order_ref": "abc123",
    "expires_in_secs": 600
  }'
# → {"ok":true,"intent_id":"pi_...","state":"created","amount_minor":1600,"expires_unix_ms":...}
```

フィールド:
- `idem_key`: **自分の注文 ID を使う**。同じ `idem_key` での再送は新しい intent を作らず、最初の応答をそのまま再生する（自分の加盟店の名前空間内でのみ有効なので、他加盟店の `idem_key` とは衝突しない）。
- `amount_minor`: **1/100 エメの整数マイナーユニット**（`1600` = 16 エメ）。1 以上 `MAX_AMOUNT`(1,000億マイナーユニット = 10億エメ) 以下。旧 `amount`（整数エメ建て）を送ると **400 `unsupported_amount_unit`**、どちらも無ければ **400 `missing_amount_minor`** で、レート制限にも冪等リプレイにも届く前に落ちる。**この改名自体が安全機構**で、キーを使い回していたら片側のデプロイが遅れたときに単位ズレが検知されずに通ってしまう（加盟店未更新なら100倍、ウォレット未更新なら1/100を請求して、どちらも商品は出る）。DEV.md §金額の単位。
- `description`: 加盟店が入力した文言として承認画面に引用枠で表示される。制御文字・bidi制御・未割当は拒否される。
- `expires_in_secs`: 省略時 600 秒。60〜1800 秒の範囲。短すぎると PIN 入力が間に合わず、長すぎると閉店後の加盟店に対する承認画面が生き続ける。
- `launch_app_id`（任意）: 決済を開始したアプリの `app_id` の自己申告。MoyMoy は OS 由来の起動元と突き合わせて不一致なら警告バナーを出す（ブロックはしない）。**Web 起点（§6「`.mnn` の Web ページから起動する場合」）では OS 由来の起動元は必ず `com.mochi.browser` になるため、自分のアプリ id を申告すると正当な決済でも警告バナーが出る。**
- `payer_hint_handle`（任意）: 支払者を `@handle` で指定する。指定すると他の口座からの承認・拒否は `payer_mismatch` で拒否される（支払者を「縛る」のであって「代わりに払わせる」のではない）。

加盟店ごとの発行上限（拘束されない売上への代償措置）: 未決済 intent 件数の上限（既定20件）と 24h 発行合計金額の上限（既定50,000エメ）。超えると `too_many_open_intents` / `daily_issue_cap` で拒否される。引き上げは `/merchant/portal/limits`（セッション + PIN）のみ。

## 6. MoyMoy を起動する

MochiOS の launch-intent 機構で MoyMoy を起動する。EC アプリ（ネイティブアプリ）からの起動は `intent_id` のみでよい。

```js
mochi.os.apps.launch("com.mochi.moymoy", { intent_id: "pi_..." });
```

自分の manifest に `apps.launch` の宣言（`ServiceScope`）が必要。**MoyMoy は `params` から `intent_id` と `return_url` の2つだけをホワイトリスト抽出し、他のキーは読まない** — 金額や加盟店名を一緒に渡しても無視される。表示は必ず moymoy-cs 自身の記録から取る。なお `return_url` が意味を持つのは下記の Web 起点のときだけで、**アプリ間起動では OS が origin を刻まないため常に捨てられる**（突き合わせる相手が無い戻り先は受け取らない）。

### `.mnn` の Web ページから起動する場合

MochiPOS のように注文サイト自体が `.mnn` の Web ページであり、アプリではない場合は、MochiOS の Web 起点 launch-intent（詳細は MochiOS 側 `docs/mobile/web-launch.md`）を使う:

```js
mochi.os.apps.launch("com.mochi.moymoy", {
  intent_id: "pi_...",
  return_url: location.href,
});
```

- ユーザー操作（クリック等）起点でなければ OS に拒否される。main frame からのみ呼べ、ページの host が `*.mnn` である必要があり、送信元 origin ごとに1秒1回のレート制限がある（詳細は MochiOS 側ドキュメント参照）。
- `return_url` は**自分のサイトのホストでなければ無視される**。OS が attest した origin とホストが一致しない URL は使われず、その場合は決済後にブラウザが前面化されるだけで元のページには戻らない。これは「加盟店が申告した戻り先を信じない」という MoyMoy の既存方針そのもので、意地悪ではなく、偽の完了画面へ誘導する経路を作らないための制約である。
- **正規化後 2048 文字**の上限がある。クエリに生の日本語を大量に載せると percent-encode で最大9倍に膨らみ（`あ` → `%E3%81%82`）上限を超え、戻れなくなる。戻り先 URL は短く保つこと。
- Web 起点では OS 由来の起動元（`from`）が `com.mochi.browser` になる（§5「`launch_app_id`」参照）。自分のアプリ id を申告すると承認画面に不一致の警告バナーが出てしまうので、Web 起点なら `com.mochi.browser` を申告するか省略すること。
- ページ側が「決済から戻ってきたこと」を知る手段は launch-intent 機構自体には無い。`visibilitychange` かバックエンドへのポーリングで検知すること。**§7の「クライアントの申告を信じない」原則はここでも一切緩まない**——決済結果は必ず自分のバックエンド経由で moymoy-cs へ照会して確定させる。

## 7. 結果を受け取る（最重要）

EC アプリが復帰したら、**自分のバックエンド経由で** moymoy-cs に照会し、`paid` を確認してから履行する。

```bash
curl https://moymoy.cs.mnn/merchant/v1/intent?intent_id=pi_... \
  -H 'Authorization: Bearer moy_sk_...'
# → {"ok":true,"intent":{"intent_id":"pi_...","state":"paid","amount_minor":1600,"payer_ref":"...","refunded":false,...}}
```

**クライアントの「払いました」という申告を信じてはならない。** 改造されたクライアントは UI 上どんな状態でも表示できるので、EC アプリ側の判断には何の意味も無い。信じてよいのはこの照会の応答だけ。

ポーリングは数秒間隔で、`expires_unix_ms` を過ぎたら打ち切って `expired` として扱ってよい（サーバー側も同じ猶予で `state: "expired"` へ遷移する）。

`payer_ref` は加盟店ごとに派生する不可逆な安定 ID。同一支払者は同一加盟店内で常に同じ値になるので再犯検知に使えるが、加盟店を跨いでは相関できない（実 handle は漏れない）。

## 7-2. QR 決済（動的・静的）

QR コードを読んで MoyMoy で払う経路。**QR は権限を運ばない** — 着地ページは `intent_id` を渡すだけで、客が払う金額・店名は §7 と同じく moymoy-cs の記録から承認画面に出る。承認後の PIN・エスクロー・履行報告は通常の決済と同じ。

### 動的 QR（請求ごとに QR を作る）

他の mod（RPSystem 等）から使う想定。

1. **事前に** §5 の `intent/create` で請求を作る。`QrBlockUseHandler` はサーバースレッド上で同期的に呼ばれ、I/O を禁止されるため、QR を使う瞬間に HTTP を呼んで請求を作ってはいけない。`launch_app_id` は**付けない**（QR 経由の起動は Web 起点なので起動元が必ず `com.mochi.browser` になり、自分のアプリ id を申告すると毎回不一致の警告バナーが出る。§5 参照）。
2. `https://moymoy.cs.mnn/qr/i/<intent_id>` を `MochiMod.QR.createItem` / `registerUseHandler` で QR に埋め込む。
3. intent の TTL は最長 30 分（§5 の `expires_in_secs`）。**期限切れの QR は使えないので、新しい intent を作って QR を作り直す。**
4. 支払い完了は、下記の待ち受け型照会で知る（Webhook は無い）。
5. `paid` を確認して商品を渡したら、`POST /merchant/v1/intent/fulfill` で履行を報告する（未報告のエスクローは 6 時間で保留される。詳細は DEV.md §エスクロー）。

```bash
curl 'https://moymoy.cs.mnn/merchant/v1/intent/wait?intent_id=pi_...&timeout_secs=20' \
  -H 'Authorization: Bearer moy_sk_...'
# → {"ok":true,"intent":{"intent_id":"pi_...","state":"created",...},"timed_out":true}
```

- `intent` は §7 の `intent`（照会）と同じ形。`state` が `created` のままなら、客が承認・拒否するか、キャンセルされるか、期限が来るか、`timeout_secs` が経つまで応答を返さない。
- `timeout_secs`: 省略時 20。1〜25 に丸められる（25 を超えても 25）。
- `timed_out: true` は「まだ `created` のまま時間切れ」。**終端ではないので、同じ呼び出しを投げ直す。** `state` が `paid` / `declined` / `canceled` / `expired` なら終端で、`timed_out` は `false`。
- 存在しない・他店の `intent_id` は 200 `unknown_intent`（§3-3）。
- **店ごとに同時 16 本まで。** 超えると 429 `too_many_waiters`（`limit` 付き）。
- 読み取りレート枠は `intent_get` と共有（120 回/60 秒、§9）。1 回の待ち受けが 1 回分を消費する。
- 結果を信じる根拠は §7 と同じ、この応答だけ。

#### Java のサンプル（`awaitPaid`）

`java.net.http.HttpClient` の `sendAsync` を使い、`CompletableFuture` を返してコールバックで受ける形。`client` は §4 の方法で `moymoy.cs.mnn` に到達できるよう構成済みのもの（Java の `HttpClient` で §4 のプロキシ認証をどう組むかは本書では扱っていない）。

```java
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;

static final Set<String> TERMINAL = Set.of("paid", "declined", "canceled", "expired");

/** intent が終端状態になったら、その intent オブジェクトで完了する。 */
static CompletableFuture<JsonObject> awaitPaid(HttpClient client, String apiKey, String intentId) {
    HttpRequest req = HttpRequest.newBuilder(URI.create(
            "https://moymoy.cs.mnn/merchant/v1/intent/wait?intent_id=" + intentId + "&timeout_secs=20"))
        .header("Authorization", "Bearer " + apiKey)
        .GET().build();
    return client.sendAsync(req, HttpResponse.BodyHandlers.ofString())
        .thenCompose(res -> {
            JsonObject body = JsonParser.parseString(res.body()).getAsJsonObject();
            if (!body.get("ok").getAsBoolean()) {
                // unknown_intent / too_many_waiters / rate_limited など。投げ直さず呼び出し側へ。
                return CompletableFuture.failedFuture(new IllegalStateException(res.body()));
            }
            JsonObject intent = body.getAsJsonObject("intent");
            if (TERMINAL.contains(intent.get("state").getAsString())) {
                return CompletableFuture.completedFuture(intent);
            }
            // timed_out で state が created のまま: もう一度待つ
            return awaitPaid(client, apiKey, intentId);
        });
}

// 使う側:
awaitPaid(client, apiKey, intentId).thenAccept(intent -> {
    // ここはサーバースレッドではない。ワールドやプレイヤーに触る処理は戻す:
    server.execute(() -> {
        if ("paid".equals(intent.get("state").getAsString())) {
            // 商品を渡す → intent/fulfill で履行を報告
        }
    });
});
```

- コールバックは HTTP クライアントのスレッドで走る。**Minecraft のメインスレッドへ戻すには `server.execute` を使う**（ワールド・プレイヤーを直接触らない）。
- 再帰の投げ直しは `ok:false`（429 含む）では行わない。失敗を無限に回さないため。レート制限に当たったら `retry_after_ms` 待ってから呼び出し側で再開する。
- 期限後に QR を読まれる場合に備え、`expired` を受けたら QR を作り直す。

### 静的 QR（店頭据え置き、客が金額を入力）

- 加盟店管理画面（§3-2 の到達手順）で「静的 QR」を発行する（セッション + PIN）。表示される URL（`https://moymoy.cs.mnn/qr/s/<code>`）を、サーバー管理者が `/mochi qr label "<店名>" <URL>` で QR ブロックにする。**再発行すると旧コードは直ちに無効になる**（印刷物の紛失・汚損時の失効手段）。停止中・閉店した店のコードは使えない。
- 客は QR を読み、店名を確認して金額を入力し、PIN で承認する。客が作る請求は店の発行上限（§5）には数えられない。
- 静的 QR の請求は客が作るため、店のシステムは請求 ID を前もって知らない。**加盟店管理画面の売上ページで、受け取った分について「渡した」報告をする**（API キーの `intent/fulfill` でも報告できるが、ID は売上ページか `/merchant/portal/sales` で知るしかない）（PIN 不要・ログイン中のセッションのみ）。API からは `POST /merchant/portal/fulfill {merchant_id, intent_id, fulfilled_amount_minor, reason?}`（セッション認証。本体と応答は `intent/fulfill` と同じ）。未報告のまま 6 時間経つと保留される。
- QR ブロックは MochiOS の Fabric 26.2 版でのみ使える。

## 8. 金額の扱い

MoyMoy が受け取るのは 1/100 エメの整数マイナーユニットだけで、浮動小数は一切入らない。小数価格を持つカタログをこの整数に変換するのは加盟店側の責任。**PiggleShop の実装が worked example**（`server/piggleshop-cs/src/orders.rs`。金額計算ロジックは実装・テスト済みだが、moymoy-cs への intent 作成呼び出しはまだ配線されていない ── §11 参照）:

1. カタログ価格を **1/100 エメ単位の整数**（"cents"）に変換してから足し算する。浮動小数のまま `0.05 + 0.05 + 0.05` を足すと `0.15000000000000002` のような誤差が乗る。
2. **その合計がそのまま `amount_minor`。整数エメへ丸め直してはいけない。** 丸めた数を `amount_minor` に入れると単位は正しいまま請求額が 1/100 になり、`unsupported_amount_unit` のような検知は一切効かない（キー名は合っているため）。
3. float → integer の変換は `truncate` ではなく `round`。**この変換は行ごとに一度だけ**行い、以降は整数で合算する。合計を float のまま持って最後に丸めると、行ごとに見せた価格の和と請求額が一致しなくなる。カタログの刻みが 1/100 エメより細かい場合、1/100 エメ未満の行は 0 に丸まりうる — その行を 0 で通すか 1 に切り上げるかは加盟店の値付けの問題で、MoyMoy 側の要求は `amount_minor >= 1`（カート全体で）だけ。

```rust
// 価格計算の唯一の float→integer 境界。`as i64` まで含めてここで整数にする
let cents = (price * 100.0).round() as i64; // truncate ではなく round — 4.25 が 4.2499999… で来ても 425
// … 全行を i64 の cents で合算 …
let amount_minor = total_cents; // 合計がそのまま送る値。÷100 も切り上げもしない
```

`amount_minor` は整数として直列化すること。`1600.0` のような float を送ると、後述の 422 で落ちる（`unsupported_amount_unit` にはならない）。

## 9. エラーとリトライ

| コード | 意味 | 対処 |
|---|---|---|
| 400 `unsupported_amount_unit` | 旧 `amount`（整数エメ建て）を送った | `amount_minor`（1/100 エメ）に直す。§5 |
| 400 `missing_amount_minor` | `amount_minor` も `amount` も無い | `amount_minor` を入れる |
| `bad_amount` | `amount_minor` が 1 未満 または `MAX_AMOUNT`(1,000億マイナーユニット=10億エメ) 超 | 金額を直す |
| `bad_description` / `bad_order_ref` | 不正な文字列（`reason` 参照） | 制御文字・bidi制御・連続する結合文字を除く |
| `bad_expires_in_secs` | TTL が 60〜1800 秒の範囲外 | 範囲内の値に直す（`min`/`max` が応答に入る） |
| `too_many_open_intents` | 未決済 intent が上限（既定20）超過 | 古い intent を `cancel` するか、上限を portal で引き上げる |
| `daily_issue_cap` | 24h 発行合計が上限（既定50,000エメ）超過 | 翌日を待つか上限を引き上げる |
| `unknown_payer_hint` | `payer_hint_handle` が存在しない handle | handle を確認する |
| `unknown_intent` | 存在しない、または他加盟店の intent_id | `intent_id` を確認する |
| `already_paid` | 既に `paid`（cancel でも履行義務は消えない） | 履行済みか確認し、二重発送しない |
| `not_cancelable` | `paid`/`canceled`/`expired` を再度 cancel した | 現在の `state` を見て判断 |
| 429 `rate_limited` | 呼び出し過多（作成30/分・照会120/分・登録は口座あたり1回/10分） | `retry_after_ms` 待って再試行（`Retry-After` ヘッダは無い） |
| 401 `invalid merchant API key` | キーが誤り・失効済み・**閉店した加盟店のもの**（§3-3） | ローテーション済みでないか確認 |
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

- **webhook は無い。** 結果はポーリング、または待ち受け型照会（§7-2）で取得する。
- **加盟店主導の返金 API は無い。** 運営による強制返金のみ（CLI 経由、加盟店 API からは呼べない）。
- **署名レシートは無い。** 照会は API キー + TLS の認証済みチャネルの応答をそのまま信頼できる前提。
- **閉店（`/merchant/portal/close`）だけはアプリ内の加盟店管理画面に UI が無い。** 登録・キー再発行・停止/再開・発行上限の変更・売上ページは画面から行える（到達手順は §3-2）。
- PiggleShop の `orders.rs` にある金額計算・状態機械は実装・テスト済みだが、**moymoy-cs への intent 作成呼び出しの配線自体は未実装**（Phase 3 待ち）。worked example として参照する際は計算ロジックのみを見ること。

受容しているリスクや設計判断の背景（加盟店売上を拘束しない理由、第二要素を持たない理由など）は `DEV.md` を参照。
