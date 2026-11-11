/* global React, CrystalIcon, EmeGem, formatEme, formatCount, MoyMoy, MoyPinStep, MoyAmountInput, useMoyAmountInput, errLabelFor, msState, msEffect, msRef, fieldStyle, ctaStyle */
/* =====================================================================
   MoyMoy — 加盟店管理 (設定内)

   moymoy-screens.jsx から切り出したもの。中身は移動しただけで変えていない。

   フックのエイリアス (msState / msEffect / msRef) と入力欄のスタイル
   (fieldStyle / ctaStyle) は、それぞれ moymoy-screens.jsx と moymoy-auth.jsx の
   トップレベル const を使う。ここで宣言し直すと、同じ名前の const がグローバル
   字句環境に二重に入って SyntaxError になる（各ファイルは別々の <script> だが
   字句環境は共有される）。読むのは描画時なので、読み込み順は問わない。
   ===================================================================== */

/* `bad_name` / `bad_sub` に添えて返る `reason`。生のコードを出すと
   （reserved_name）のような英語がそのまま日本語画面に出るので、文にする。 */
const REASON_LABEL = {
  empty: "入力してください",
  too_long: "長すぎます",
  invisible_char: "表示されない文字が含まれています",
  stacked_marks: "装飾記号が多すぎます",
  reserved_name: "運営や公式を連想させる語は使えません",
  mixed_script: "複数の文字体系が混ざっています",
  unnameable: "名前として識別できません",
};

/* 店名・ひとこと説明の長さを、サーバ (merchant.rs の sanitize_text) と同じ数え方で
   測る。あちらは「NFKC を掛ける → その後で trim する → Unicode スカラー値を
   `chars().count()` で数える」順であり、JS でこれに 1 対 1 で対応する式が
   `Array.from(s.normalize("NFKC").trim()).length`。

   素の `.length` は UTF-16 コード単位なので、二方向に食い違う:
     - 過小: NFKC は縮むだけでなく伸びる。㈱→(株) は 1→3、㍿→株式会社 は 1→4、
       ℡→TEL、㎡→m2、№→No、㌔→キロ。「㈱」+ あ×31 は `.length` では 32 でも
       サーバでは 34 で、bad_name / too_long で弾かれる。日本の店名で日常的に踏む。
     - 過大: サロゲートペアは 2 と数えるので、絵文字の店名は 16 個で頭打ちになる。
       サーバは 32 個まで受理する。
   Array.from はサロゲートペアを 1 個として数えるので `chars().count()` と一致する。

   ここで数えるのは長さだけ。reserved_name / mixed_script / unnameable 等の判定は
   サーバが権威で、UTS #39 の skeleton をブラウザで再現することはできない
   （食い違えば嘘の警告になる）ので、こちらには持ち込まない。 */
function moyTextLen(s) {
  return Array.from(s.normalize("NFKC").trim()).length;
}

/* ─── 加盟店管理 (設定内) ─────────────────────────────────────────
   セッション + PIN でしか触れない側の操作。API キーが漏れてもここには届かず、
   セッションが漏れても PIN 無しではキーを再発行できない、という分離が
   バックエンド側で引かれているので、UI もその 2 系統を混ぜない。 */
const MOY_STATUS_LABEL = { active: "稼働中", disabled: "停止中", deleted: "閉店" };

function moyDateLabel(ts) {
  if (!ts) return "—";
  const d = new Date(ts);
  return d.getFullYear() + "/" + (d.getMonth() + 1) + "/" + d.getDate() + " " +
    d.toLocaleTimeString("ja-JP", { hour: "2-digit", minute: "2-digit" });
}

/* ─── 売上 (エスクロー状況) ────────────────────────────────────────
   決済モデルが「承認と同時に送金」から「エスクローで保留 → 履行報告 →
   10分ゲート後に送金」に変わったことで、加盟店オーナーに新しい問い
   （「買い手は払ったのに残高が増えていない、金はどこにあるのか」）が
   生まれた。この一群はそれに答えるためのもの。 */

/* stage === "none" はエスクロー対象外（未決済扱いの決済 / エスクロー導入前の
   決済）で、どちらも正常。ここに異常のバッジを出すと、既存の正常な決済まで
   壊れて見える。だからラベルを持たせず、バッジ自体を描かない。

   held と parked は違う状態。held は「支払い済み・履行待ち」で正常な進行中、
   parked は「6時間の自動報告期限を過ぎ、自動返金もされず、人の判断待ちで
   止まっている」— 加盟店が能動的に確認すべき状態。held の薄い網掛けに対し、
   parked は carle-red の塗りつぶしにして、一見して別物と分かるようにする
   （held より強い表示にする、という要求への対応）。 */
const ESCROW_STAGE = {
  held:      { label: "入金保留中",            border: "var(--carle-red)", bg: "rgba(227,38,54,0.08)", color: "var(--carle-red)" },
  parked:    { label: "入金停止中（要確認）",  border: "var(--carle-red)", bg: "var(--carle-red)",     color: "#fff" },
  fulfilled: { label: "報告済み・送金待ち",    border: "var(--ink)",       bg: "rgba(0,0,0,0.05)",     color: "var(--ink)" },
  released:  { label: "入金済み",              border: "var(--moy-deep)",  bg: "var(--moy-mint)",      color: "var(--moy-deep)" },
};
function EscrowStageBadge({ stage }) {
  const s = ESCROW_STAGE[stage];
  if (!s) return null;
  return (
    <span style={{ border: "1.5px solid " + s.border, background: s.bg, color: s.color,
      padding: "1px 6px", fontFamily: "var(--font-jp)", fontSize: 10, fontWeight: 700,
      whiteSpace: "nowrap" }}>{s.label}</span>
  );
}

/* 履行の報告（「渡した」）。held の行にだけ出す。
   PIN は求めない: この報告は加盟店が受け取る額を減らす方向にしか効かず
   （サーバ側の取り決め）、読むだけの売上ページと同じ「セッションのみ」の信頼
   水準に置いてある。そのかわり取り消せないので、送る前に「入金される額 / 買い手へ
   返金される額」を必ず見せる。

   結果は行ごとにこの部品の state に持つ。報告後に売上を取り直しても行（key =
   intent_id）は作り直されないので、「報告しました」の表示は残る。 */
const MOY_MAX_FULFIL_REASON = 120;

function FulfillPanel({ merchantId, sale, held, onReported }) {
  const [open, setOpen] = msState(false);
  const [partial, setPartial] = msState(false);
  const [reason, setReason] = msState("");
  const [busy, setBusy] = msState(false);
  const [result, setResult] = msState(null); // {ok, text} | null
  const entry = useMoyAmountInput(true);
  const aliveRef = msRef(true);
  msEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);

  const full = sale.amount_minor;
  const reasonLen = moyTextLen(reason);
  const reasonOk = reasonLen <= MOY_MAX_FULFIL_REASON;
  // 一部報告の金額は 0〜支払い額（サーバの bad_fulfilled_amount と同じ範囲）。
  // 空欄や「.」だけは数字が無いので、0 と読み替えて送らず入力待ちにする。
  const partialOk = /\d/.test(entry.text) && !entry.error && entry.amount !== null && entry.amount <= full;
  const amountOk = !partial || partialOk;
  const fulfilled = partial ? (partialOk ? entry.amount : 0) : full;
  const canSend = !busy && amountOk && reasonOk;

  // 詳細パネルからの報告（全額 / 一部、理由つき）。
  function send() {
    if (!canSend) return;
    return submit(fulfilled, reason.trim());
  }
  // 「全額渡した」の 1 タップ報告。確認画面は挟まない（店頭で商品を手渡しした
  // 直後に押す主操作で、毎回の確認は運用の邪魔になる）。そのかわり、押下から結果が
  // 返るまで busy で無効にして二重送信を防ぎ、結果は必ず行に出す。
  function sendFull() {
    if (busy) return;
    return submit(full, "");
  }

  async function submit(fulfilledAmount, reasonText) {
    setBusy(true); setResult(null);
    let text;
    let ok = false;
    // サーバ側で状態が変わっている応答 (報告済み / もう held ではない) のあとも
    // 売上を取り直す — 手元の行が古いままだと「渡した」が出続ける。
    let refresh = false;
    try {
      const r = await MoyMoy.portalFulfill(merchantId, sale.intent_id, fulfilledAmount, reasonText);
      if (r.ok) {
        ok = true; refresh = true;
        const refunded = typeof r.refund_amount_minor === "number" ? r.refund_amount_minor : full - fulfilledAmount;
        const paid = typeof r.fulfilled_amount_minor === "number" ? r.fulfilled_amount_minor : fulfilledAmount;
        text = "履行を報告しました。入金される額 " + formatEme(paid) + " エメ" +
          (refunded > 0 ? "、買い手への返金 " + formatEme(refunded) + " エメ" : "") + "。";
      } else if (r.error === "already_fulfilled") {
        refresh = true;
        text = errLabelFor({ code: r.error }) +
          (typeof r.fulfilled_amount_minor === "number" ? "（入金される額 " + formatEme(r.fulfilled_amount_minor) + " エメ）" : "") + "。";
      } else if (r.error === "not_held") {
        refresh = true;
        const stage = ESCROW_STAGE[r.escrow_stage];
        text = errLabelFor({ code: r.error }) + (stage ? "（現在: " + stage.label + "）" : "") + "。";
      } else if (r.error === "bad_reason") {
        text = errLabelFor({ code: r.error }) +
          (r.max_chars ? "（" + r.max_chars + "文字まで）" : "") +
          (r.reason_code && REASON_LABEL[r.reason_code] ? "（" + REASON_LABEL[r.reason_code] + "）" : "");
      } else {
        text = errLabelFor({ code: r.error, retry_after_ms: r.retry_after_ms });
      }
    } catch (e) {
      console.error("MoyMoy: merchant fulfil failed", e);
      text = errLabelFor({ code: "network" });
    }
    if (!aliveRef.current) return;
    setBusy(false);
    setResult({ ok, text });
    if (ok || refresh) setOpen(false);
    // 結果の文は呼び出し側にも渡す: 一覧が絞り込み（渡し待ち）だと、報告した行は
    // 取り直しで消え、この部品ごと無くなって結果が見えなくなるため。
    if (refresh && onReported) onReported({ ok, text });
  }

  if (!held && !result) return null;
  return (
    <div style={{ marginTop: 8 }}>
      {result && (
        <div role={result.ok ? "status" : "alert"} style={{ marginBottom: 8, padding: "8px 10px",
          border: "1.5px solid " + (result.ok ? "var(--moy-deep)" : "var(--carle-red)"),
          background: result.ok ? "var(--moy-mint)" : "rgba(227,38,54,0.08)",
          color: result.ok ? "var(--moy-deep)" : "var(--carle-red)",
          fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, lineHeight: 1.6 }}>{result.text}</div>
      )}
      {held && !open && (
        <>
          <button disabled={busy} onClick={sendFull} style={{ width: "100%", padding: "13px",
            border: "1.5px solid var(--moy-deep)", background: busy ? "#e6e6e6" : "var(--moy-deep)",
            color: busy ? "var(--ink-soft)" : "#fff", cursor: busy ? "default" : "pointer",
            fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 15 }}>
            {busy ? "報告しています…" : "全額渡した"}
          </button>
          <button disabled={busy} onClick={() => { setPartial(true); setOpen(true); }}
            style={{ display: "block", margin: "6px auto 0", padding: "6px 8px", border: "none",
            background: "transparent", cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)",
            fontSize: 12, fontWeight: 600, color: "var(--ink-soft)", textDecoration: "underline" }}>
            一部だけ渡した…
          </button>
        </>
      )}
      {held && open && (
        <div style={{ border: "1.5px solid var(--ink)", padding: "12px", background: "var(--bg-white)" }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 800, color: "var(--ink)" }}>
            商品を渡したことを報告します
          </div>
          <div style={{ marginTop: 8, display: "grid", gridTemplateColumns: "1fr 1fr", border: "1.5px solid var(--ink)" }}>
            {[[false, "全額渡した"], [true, "一部だけ渡した"]].map(([isPartial, label], i) => (
              <button key={label} disabled={busy} onClick={() => setPartial(isPartial)} style={{
                padding: "9px 0", border: "none", borderLeft: i === 0 ? "none" : "1.5px solid var(--ink)",
                background: partial === isPartial ? "var(--moy-deep)" : "var(--bg-white)",
                color: partial === isPartial ? "#fff" : "var(--ink)", cursor: busy ? "default" : "pointer",
                fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>{label}</button>
            ))}
          </div>
          {partial && (
            <div style={{ marginTop: 12 }}>
              <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>実際に渡した分の金額</div>
              <MoyAmountInput entry={entry} id={"moy-fulfil-" + sale.intent_id} label="実際に渡した分の金額" size={30} />
              <div style={{ marginTop: 4, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)",
                textAlign: "center", lineHeight: 1.6 }}>
                0 から {formatEme(full)} エメまで。差額は買い手へ返金されます。
              </div>
            </div>
          )}
          <div style={{ marginTop: 12 }}>
            <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>理由（任意）</div>
            <input value={reason} disabled={busy} onChange={(e) => setReason(e.target.value)}
              maxLength={MOY_MAX_FULFIL_REASON * 3} aria-label="報告の理由（任意・120文字まで）"
              placeholder="例: 在庫切れのため一部のみ" style={fieldStyle} />
            <div style={{ marginTop: 4, fontFamily: "var(--font-jp)", fontSize: 11, lineHeight: 1.6,
              color: reasonOk ? "var(--ink-soft)" : "var(--carle-red)", fontWeight: reasonOk ? 400 : 700 }}>
              {reasonLen} / {MOY_MAX_FULFIL_REASON} 文字{reasonOk ? "" : "。長すぎます"}
            </div>
          </div>
          <div style={{ marginTop: 12, padding: "8px 10px", border: "1px dashed rgba(0,0,0,0.28)",
            fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink)", lineHeight: 1.7 }}>
            <div style={{ display: "flex", justifyContent: "space-between" }}>
              <span>入金される額</span>
              <span style={{ fontWeight: 700 }}>{amountOk ? formatEme(fulfilled) + " エメ" : "—"}</span>
            </div>
            <div style={{ display: "flex", justifyContent: "space-between" }}>
              <span>買い手への返金</span>
              <span style={{ fontWeight: 700 }}>{amountOk ? formatEme(full - fulfilled) + " エメ" : "—"}</span>
            </div>
          </div>
          <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, fontWeight: 700,
            color: "var(--carle-red)", lineHeight: 1.6 }}>
            報告は取り消せません。内容を確かめてから送ってください。
          </div>
          <button disabled={!canSend} onClick={send} style={{ ...ctaStyle(!canSend), marginTop: 10 }}>
            {busy ? "報告しています…" : "この内容で報告する"}
          </button>
          <button disabled={busy} onClick={() => setOpen(false)} style={{ width: "100%", marginTop: 6,
            padding: "10px", border: "none", background: "transparent", cursor: busy ? "default" : "pointer",
            fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>やめる</button>
        </div>
      )}
    </div>
  );
}

/* 支払い済みの1件について、エスクローの状況（保留・報告済み・入金済み・停止）と
   「渡した」の報告、部分履行・返金の表示。売上ページの行と請求管理ページの行で
   同じ表示を使うため、行の見出しから切り出してある。 */
function SaleEscrowNotes({ sale, merchantId, onReported }) {
  const escrow = sale.escrow || { stage: "none" };
  // 部分履行（一部だけ届けられず差額が返金された）かどうかは、返金額の
  // 有無で判定する。fulfilled_amount_minor だけでは「満額履行」と区別できない。
  const partial = (escrow.refunded_amount_minor || 0) > 0;
  return (
    <>
      {escrow.stage === "held" && (
        <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.6 }}>
          買い手はすでに支払い済みです。商品を渡して履行を報告すると、
          {escrow.release_due_unix_ms ? moyDateLabel(escrow.release_due_unix_ms) + " 以降に" : "しばらくして"}
          この口座へ入金されます。
        </div>
      )}
      {/* 「渡した」。held のときだけ押せる。報告後の結果表示は、売上を取り直して
          stage が変わったあとも残す（FulfillPanel が result を持つ）。 */}
      <FulfillPanel merchantId={merchantId} sale={sale} held={escrow.stage === "held"} onReported={onReported} />
      {/* parked: 6時間の報告期限を過ぎても履行報告が無く、かつ（旧仕様と違い）
          自動返金もされずに止まっている状態。ここに書けるのは事実だけ:
          代金は消えていないこと、自動では動かないこと、いま報告しても
          not_held で弾かれる（すでに held の段階を過ぎている）ため無意味な
          ことだけ。「報告すれば入金される」も「払い出される」も断言しない
          — 払い出し経路が実在しないため、そう読める文言を書かない。 */}
      {escrow.stage === "parked" && (
        <div style={{ marginTop: 8, padding: "8px 10px", border: "1.5px solid var(--carle-red)",
          background: "rgba(227,38,54,0.06)", fontFamily: "var(--font-jp)", fontSize: 11,
          color: "var(--ink)", lineHeight: 1.7 }}>
          {escrow.parked_unix_ms && (
            <div style={{ fontWeight: 700, color: "var(--carle-red)" }}>
              {moyDateLabel(escrow.parked_unix_ms)} から、保留のまま止まっています。
            </div>
          )}
          代金は買い手から引き落とされ、MoyMoy が保持しています（消えたわけではありません）。
          期限内に履行の報告が届かなかったため、入金は自動では進みません。この注文は
          すでに保留の段階を過ぎており、いま履行を報告しても受け付けられません。
          MoyMoy 側での確認が必要です。
        </div>
      )}
      {escrow.stage === "fulfilled" && escrow.release_due_unix_ms && (
        <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.6 }}>
          履行の報告は受け付けています。{moyDateLabel(escrow.release_due_unix_ms)} 以降に入金されます。
        </div>
      )}
      {escrow.stage === "released" && escrow.released_unix_ms && (
        <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)" }}>
          {moyDateLabel(escrow.released_unix_ms)} に入金済みです。
        </div>
      )}

      {partial && (
        <div style={{ marginTop: 8, padding: "8px 10px", border: "1px dashed rgba(0,0,0,0.28)",
          fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink)", lineHeight: 1.7 }}>
          一部のみ履行されました。差額は買い手へ返金されています。
          <div style={{ display: "flex", justifyContent: "space-between", marginTop: 4 }}>
            <span>入金される額</span>
            <span style={{ fontWeight: 700 }}>{formatEme(escrow.fulfilled_amount_minor)} エメ</span>
          </div>
          <div style={{ display: "flex", justifyContent: "space-between" }}>
            <span>返金額</span>
            <span style={{ fontWeight: 700 }}>{formatEme(escrow.refunded_amount_minor)} エメ</span>
          </div>
          {escrow.fulfil_reason && (
            <div style={{ marginTop: 4, color: "var(--ink-soft)" }}>理由: {escrow.fulfil_reason}</div>
          )}
        </div>
      )}

      {sale.refunded && (
        <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, fontWeight: 700, color: "var(--carle-red)" }}>
          この決済は取り消され、買い手へ返金されています
        </div>
      )}

      {/* payer_ref はソルト付きの擬似 ID。買い手を特定できる情報ではないので、
          そう読めるラベルは付けない — 問い合わせの突き合わせ用の技術参照値として
          小さく添えるだけ。 */}
      {sale.payer_ref && (
        <div style={{ marginTop: 6, fontFamily: "var(--font-mono)", fontSize: 9, color: "var(--ink-soft)", opacity: 0.7 }}>
          支払い参照（匿名化） {sale.payer_ref}
        </div>
      )}
    </>
  );
}

/* 1件の売上行。金額はすべて minor — formatEme を必ず経由する。 */
function SalesRow({ sale, last, merchantId, onReported }) {
  const escrow = sale.escrow || { stage: "none" };
  return (
    <div style={{ padding: "12px 14px", background: "var(--bg-white)",
      borderBottom: last ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
      <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 10 }}>
        <div style={{ minWidth: 0 }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700, color: "var(--ink)",
            whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
            {sale.description || "（内容の記載なし）"}
          </div>
          <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)", marginTop: 2,
            display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{moyDateLabel(sale.created_unix_ms)}</span>
            {/* 店頭 QR 経由の売上。API 経由 (source: "api") は従来どおり何も付けない。 */}
            {sale.source === "static_qr" && (
              <span style={{ border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
                color: "var(--moy-deep)", padding: "0 5px", fontFamily: "var(--font-jp)", fontSize: 10,
                fontWeight: 700, whiteSpace: "nowrap" }}>店頭 QR</span>
            )}
          </div>
        </div>
        <div style={{ textAlign: "right", flexShrink: 0 }}>
          <div style={{ fontFamily: "var(--font-mono)", fontWeight: 700, fontSize: 15, color: "var(--ink)" }}>
            {formatEme(sale.amount_minor)} エメ
          </div>
          <div style={{ marginTop: 4 }}><EscrowStageBadge stage={escrow.stage} /></div>
        </div>
      </div>

      <SaleEscrowNotes sale={sale} merchantId={merchantId} onReported={onReported} />
    </div>
  );
}

/* 発行直後の 1 回だけ見せる API キー。DB にはハッシュしか無いので、この画面を
   閉じたら本当に二度と出せない。それを画面上で明言する。 */
function MoyApiKeyReveal({ apiKey, onClose }) {
  return (
    <div style={{ marginTop: 12, border: "1.5px solid var(--carle-red)", padding: "12px 14px",
      background: "rgba(227,38,54,0.05)" }}>
      <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 800, color: "var(--carle-red)" }}>
        このキーを表示できるのは今この 1 回だけです
      </div>
      <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink)",
        lineHeight: 1.7 }}>
        いま控えてください。保存されているのはハッシュだけなので、MoyMoy 側から
        再表示することはできません。無くしたときは「キーを再発行」で作り直します。
      </div>
      <div style={{ marginTop: 10, padding: "10px 12px", border: "1.5px solid var(--ink)",
        background: "var(--bg-white)", fontFamily: "var(--font-mono)", fontSize: 12,
        wordBreak: "break-all", userSelect: "all" }}>{apiKey}</div>
      <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--carle-red)",
        fontWeight: 700, lineHeight: 1.7 }}>
        これは秘密情報です。ソースコードに直接書かず、サーバの環境変数に置いて
        ください。公開リポジトリに入れた場合はすぐ再発行してください。
      </div>
      <button onClick={onClose} style={{ width: "100%", marginTop: 12, padding: "12px",
        border: "1.5px solid var(--ink)", background: "var(--bg-white)", cursor: "pointer",
        fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 14 }}>
        控えました（閉じる）
      </button>
    </div>
  );
}

/* クリップボードへ書く。失敗は握りつぶさない — 呼び出し側が画面に出す。
   `navigator.clipboard` が無い・拒否された環境では例外になる（どちらも同じ catch）。 */
async function moyCopyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    console.error("MoyMoy: clipboard write failed", e);
    return false;
  }
}

function MoyCopyButton({ text, label }) {
  const [state, setState] = msState(null); // "ok" | "fail" | null
  const aliveRef = msRef(true);
  msEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);
  async function copy() {
    const ok = await moyCopyText(text);
    if (aliveRef.current) setState(ok ? "ok" : "fail");
  }
  return (
    <>
      <button onClick={copy} style={{ marginTop: 6, padding: "8px 14px", border: "1.5px solid var(--ink)",
        background: "var(--bg-white)", cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700,
        fontSize: 13 }}>{label || "コピー"}</button>
      {state === "ok" && (
        <span role="status" style={{ marginLeft: 10, fontFamily: "var(--font-jp)", fontSize: 12,
          fontWeight: 700, color: "var(--moy-deep)" }}>コピーしました</span>
      )}
      {state === "fail" && (
        <div role="alert" style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
          color: "var(--carle-red)", lineHeight: 1.6 }}>
          コピーできませんでした。上の文字を長押しして選択してください。
        </div>
      )}
    </>
  );
}

/* コマンドの引用符つき引数に入れる文字列。Minecraft（Brigadier）の引用文字列は
   `\"` と `\\` を解釈するので、店名の `"` と `\` をエスケープしておかないと、
   店名の途中で引数が終わってコマンドが壊れる。 */
function moyQuoteArg(s) {
  return '"' + String(s).replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"';
}

/* 店ごとの「店頭 QR（静的 QR）」。発行・作り直しは PIN 付きの操作で、PIN の
   集め方は他の操作（キー再発行など）と同じく親の askPin が持つ。ここは表示だけ。
   URL は発行直後も一覧 (`static_qr_url`) から取り直して出す。 */
function MoyStaticQrSection({ merchant, onIssue }) {
  const url = merchant.static_qr_url || null;
  const active = merchant.status === "active";
  const command = url ? "/mochi qr label " + moyQuoteArg(merchant.name) + " " + url : "";
  return (
    <div style={{ marginTop: 22 }}>
      <div className="h-section">店頭 QR（静的 QR）</div>
      <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11,
        color: "var(--ink-soft)", lineHeight: 1.7 }}>
        店頭の QR ブロックに設定する QR コードです。お客さまは読み取ると、このお店の名前を
        確かめたうえで、自分で金額を入力して支払います。
      </div>

      {!url && (
        <button disabled={!active} onClick={() => onIssue(merchant, false)} style={{ width: "100%", marginTop: 10,
          padding: "14px", border: "1.5px solid var(--ink)", background: active ? "var(--bg-white)" : "#e6e6e6",
          boxShadow: active ? "3px 3px 0 var(--ink)" : "none", cursor: active ? "pointer" : "default",
          fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15 }}>
          店頭 QR を発行する
        </button>
      )}
      {!url && !active && (
        <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
          稼働中のお店だけ発行できます。
        </div>
      )}

      {url && (
        <>
          <div style={{ marginTop: 10, padding: "10px 12px", border: "1.5px solid var(--ink)",
            background: "var(--bg-white)", fontFamily: "var(--font-mono)", fontSize: 12,
            wordBreak: "break-all", userSelect: "all" }}>{url}</div>
          <MoyCopyButton text={url} label="URL をコピー" />

          <div style={{ marginTop: 14, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--ink)" }}>
            QR ブロックに設定する（ゲームマスター向け）
          </div>
          <div style={{ marginTop: 4, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
            QR ブロックを設置する権限のある人が、次のコマンドを実行します。
          </div>
          <div style={{ marginTop: 6, padding: "10px 12px", border: "1.5px solid var(--ink)",
            background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-mono)", fontSize: 12,
            wordBreak: "break-all", userSelect: "all" }}>{command}</div>
          <MoyCopyButton text={command} label="コマンドをコピー" />
          <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
            QR ブロックは Fabric 26.2 版のサーバーでのみ使えます。
          </div>

          <button disabled={!active} onClick={() => onIssue(merchant, true)} style={{ width: "100%", marginTop: 14,
            padding: "13px", border: "1.5px solid var(--carle-red)", background: "rgba(227,38,54,0.06)",
            cursor: active ? "pointer" : "default", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 14,
            color: "var(--carle-red)" }}>
            作り直す（古い QR は使えなくなります）
          </button>
        </>
      )}
      {!url && (
        <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
          QR ブロックは Fabric 26.2 版のサーバーでのみ使えます。
        </div>
      )}
    </div>
  );
}

/* ─── 請求管理 ───────────────────────────────────────────────────
   お店が自分で請求（payment intent）を作り、QR ブロックに載せて集金するための
   ページ。作成・取り消しはセッションだけで通る（PIN なし）: 請求は「払ってください」
   という依頼にすぎず、実際に金が動くのは支払う側が自分の PIN で承認したときだけ。
   店頭で商品を手渡しする運用では、支払いのあとの「渡した」の報告がいちばん大事な
   操作なので、フィルタの既定は「渡し待ち」にしてある。 */

const MOY_MAX_BILL_DESC = 120;
const MOY_MAX_BILL_REF = 64;
const MOY_BILL_TTLS = [[300, "5 分"], [600, "10 分"], [1800, "30 分"]];

/* 行の `state`（サーバが期限切れまで反映した実効状態）。 */
const MOY_BILL_STATE = {
  created:  { label: "未払い",   border: "var(--ink)",       bg: "rgba(0,0,0,0.05)",     color: "var(--ink)" },
  paid:     { label: "支払済み", border: "var(--moy-deep)",  bg: "var(--moy-mint)",      color: "var(--moy-deep)" },
  expired:  { label: "期限切れ", border: "rgba(0,0,0,0.35)", bg: "transparent",          color: "var(--ink-soft)" },
  canceled: { label: "取消",     border: "rgba(0,0,0,0.35)", bg: "transparent",          color: "var(--ink-soft)" },
  declined: { label: "拒否",     border: "var(--carle-red)", bg: "rgba(227,38,54,0.08)", color: "var(--carle-red)" },
};
const MOY_BILL_SOURCE = { api: "API", static_qr: "店頭 QR", portal: "アプリ" };
const MOY_BILL_FILTERS = [
  { key: "held",     label: "渡し待ち", empty: "渡し待ちの請求はありません" },
  { key: "open",     label: "未払い",   empty: "未払いの請求はありません" },
  { key: "paid",     label: "支払済み", empty: "支払済みの請求はありません" },
  { key: "expired",  label: "期限切れ", empty: "期限切れの請求はありません" },
  { key: "canceled", label: "取消",     empty: "取り消した請求はありません" },
  { key: "all",      label: "すべて",   empty: "まだ請求がありません" },
];

const MOY_BILL_ERR = {
  merchant_not_active: "このお店は現在稼働していないため、請求を作成できません",
  bad_amount: "金額が不正です。0.01 エメ以上で、入力上限以内にしてください",
  missing_amount_minor: "金額を入力してください",
  already_paid: "この請求はすでに支払われているため、取り消せません。商品を渡したら「渡し待ち」から報告してください",
  not_cancelable: "この請求は取り消せない状態です",
  unknown_intent: "この請求が見つかりません",
};
const MOY_BILL_NOT_CANCELABLE = {
  expired: "この請求はすでに期限が切れています",
  canceled: "この請求はすでに取り消されています",
  declined: "この請求は支払う側が断っています",
};

/* 請求の作成・取り消し・一覧の `{ok:false,...}` 応答を文にする。上限系は応答に
   添えて返る数値（limit / issued / min / max）を文に入れる。金額系 (daily_issue_cap)
   は minor なので formatEme を通し、件数 (too_many_open_intents) は通さない。 */
function moyBillErrText(r) {
  const code = (r && r.error) || "error";
  const reason = r && r.reason ? (REASON_LABEL[r.reason] || r.reason) : null;
  if (code === "too_many_open_intents") {
    return "未払いの請求が上限（" + formatCount(r.limit) + " 件）に達しています。古い請求を取り消すか、上限を引き上げてください。";
  }
  if (code === "daily_issue_cap") {
    return "24時間の発行上限（" + formatEme(r.limit) + " エメ）に達しています。" +
      (typeof r.issued === "number" ? "発行済みは " + formatEme(r.issued) + " エメです。" : "") +
      "上限を引き上げるか、時間をおいてから作成してください。";
  }
  if (code === "bad_description") return "請求の内容を確認してください" + (reason ? "（" + reason + "）" : "");
  if (code === "bad_order_ref") return "注文番号を確認してください" + (reason ? "（" + reason + "）" : "");
  if (code === "bad_expires_in_secs") {
    return "有効期限は " + Math.round(r.min / 60) + " 分から " + Math.round(r.max / 60) + " 分の間で指定してください";
  }
  if (code === "not_cancelable" && MOY_BILL_NOT_CANCELABLE[r.state]) return MOY_BILL_NOT_CANCELABLE[r.state];
  if (MOY_BILL_ERR[code]) return MOY_BILL_ERR[code];
  return errLabelFor({ code, retry_after_ms: r && r.retry_after_ms });
}

function MoyBillStateBadge({ state }) {
  const s = MOY_BILL_STATE[state];
  if (!s) return null;
  return (
    <span style={{ border: "1.5px solid " + s.border, background: s.bg, color: s.color,
      padding: "1px 6px", fontFamily: "var(--font-jp)", fontSize: 10, fontWeight: 700,
      whiteSpace: "nowrap" }}>{s.label}</span>
  );
}

/* 「渡し待ち N 件」。お店の一覧・詳細に出して、請求管理ページ（渡し待ち）へ導く。
   `held` = {n, more} または {failed:true}。more は取得件数の上限で切れている（N 件以上）。
   failed は件数を取れなかった状態: 「0 件」と同じ見た目（何も出さない）にすると、渡し待ちが
   あるのに無いように見える。取れなかったと明示して、確認先（請求管理）へ導く。 */
function MoyHeldAlert({ held, onClick }) {
  if (!held) return null;
  if (held.failed) {
    return (
      <button onClick={onClick} style={{ width: "100%", marginTop: 8, padding: "10px 14px",
        border: "1.5px solid var(--ink)", background: "rgba(0,0,0,0.04)", color: "var(--ink)",
        cursor: "pointer", display: "flex", alignItems: "center", justifyContent: "space-between", gap: 10,
        fontFamily: "var(--font-jp)", textAlign: "left" }}>
        <span style={{ fontWeight: 700, fontSize: 13 }}>渡し待ちの件数を確認できませんでした</span>
        <span style={{ fontWeight: 700, fontSize: 11 }}>請求管理で確認する ›</span>
      </button>
    );
  }
  if (!held.n) return null;
  return (
    <button onClick={onClick} style={{ width: "100%", marginTop: 8, padding: "11px 14px",
      border: "1.5px solid var(--carle-red)", background: "var(--carle-red)", color: "#fff",
      cursor: "pointer", display: "flex", alignItems: "center", justifyContent: "space-between", gap: 10,
      fontFamily: "var(--font-jp)", textAlign: "left" }}>
      <span style={{ fontWeight: 800, fontSize: 15 }}>渡し待ち {held.n}{held.more ? " 件以上" : " 件"}</span>
      <span style={{ fontWeight: 700, fontSize: 11, opacity: 0.95 }}>商品を渡したら報告する ›</span>
    </button>
  );
}

/* 請求の QR の URL と、QR ブロックに設定するコマンド。作成直後のカードと、未払い行の
   「QR を表示」で同じものを出す。 */
function MoyBillQr({ qrUrl, description }) {
  const command = "/mochi qr label " + moyQuoteArg(description) + " " + qrUrl;
  return (
    <div>
      <div style={{ marginTop: 10, padding: "10px 12px", border: "1.5px solid var(--ink)",
        background: "var(--bg-white)", fontFamily: "var(--font-mono)", fontSize: 12,
        wordBreak: "break-all", userSelect: "all" }}>{qrUrl}</div>
      <MoyCopyButton text={qrUrl} label="URL をコピー" />
      <div style={{ marginTop: 12, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--ink)" }}>
        QR ブロックに設定する（ゲームマスター向け）
      </div>
      <div style={{ marginTop: 6, padding: "10px 12px", border: "1.5px solid var(--ink)",
        background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-mono)", fontSize: 12,
        wordBreak: "break-all", userSelect: "all" }}>{command}</div>
      <MoyCopyButton text={command} label="コマンドをコピー" />
      <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
        期限が過ぎた QR は使えません。QR ブロックは Fabric 26.2 版のサーバーでのみ使えます。
      </div>
    </div>
  );
}

/* 新しい請求のフォーム。閉じると（成功・やめる）この部品ごと消えるので、入力値と
   再試行用の鍵は次に開いたときに持ち越されない。 */
function MoyNewBillForm({ merchantId, onCreated, onCancel }) {
  const entry = useMoyAmountInput(true);
  const [desc, setDesc] = msState("");
  const [ttl, setTtl] = msState(600);
  const [orderRef, setOrderRef] = msState("");
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState(null); // 応答そのもの ({error, ...})
  // 直近に送った入力と idem_key。通信が切れて応答を取り逃したときの再試行は、入力が
  // 同じなら同じ鍵で送り直し、サーバが同じ請求を返す（二重発行しない）。入力を
  // 変えたら必ず新しい鍵にする — 使い回すと古い入力の請求が返りうる。
  const attemptRef = msRef(null);
  const aliveRef = msRef(true);
  msEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);

  const descTrim = desc.trim();
  const descLen = moyTextLen(desc);
  const descOk = descLen >= 1 && descLen <= MOY_MAX_BILL_DESC;
  const descBad = descTrim !== "" && !descOk;
  const refTrim = orderRef.trim();
  const refLen = refTrim ? moyTextLen(refTrim) : 0;
  const refOk = refTrim === "" || (refLen >= 1 && refLen <= MOY_MAX_BILL_REF);
  const amountOk = /\d/.test(entry.text) && !entry.error && entry.amount !== null && entry.amount > 0;
  const canSend = !busy && amountOk && descOk && refOk;

  async function send() {
    if (!canSend) return;
    const input = { amountMinor: entry.amount, description: descTrim, orderRef: refTrim, expiresInSecs: ttl };
    const sig = JSON.stringify([input.amountMinor, input.description, input.orderRef, input.expiresInSecs]);
    if (!attemptRef.current || attemptRef.current.sig !== sig) {
      attemptRef.current = { sig, key: MoyMoy.newIdem() };
    }
    setBusy(true); setErr(null);
    try {
      const r = await MoyMoy.portalIntentCreate(merchantId, input, attemptRef.current.key);
      if (!aliveRef.current) return;
      if (r.ok && typeof r.qr_url === "string" && r.qr_url) {
        onCreated(r, input.description);
        return;
      }
      if (r.ok) {
        console.error("MoyMoy: intent create answered ok without a qr_url", r);
        setErr({ error: "error" });
      } else {
        setErr(r);
      }
    } catch (e) {
      console.error("MoyMoy: intent create failed", e);
      if (!aliveRef.current) return;
      setErr({ error: "network" });
    }
    setBusy(false);
  }

  return (
    <div style={{ marginTop: 14, border: "1.5px solid var(--ink)", padding: "14px", background: "var(--bg-white)" }}>
      <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 800, color: "var(--ink)" }}>新しい請求</div>

      <div style={{ marginTop: 12 }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>金額</div>
        <MoyAmountInput entry={entry} id="moy-bill-amount" label="請求する金額" size={34} />
      </div>

      <div style={{ marginTop: 14 }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>内容（必須）</div>
        <input value={desc} disabled={busy} onChange={(e) => setDesc(e.target.value)}
          maxLength={MOY_MAX_BILL_DESC * 3} aria-label="請求の内容（120文字まで）"
          placeholder="例: 石レンガ x64" style={fieldStyle} />
        <div style={{ marginTop: 4, fontFamily: "var(--font-jp)", fontSize: 11, lineHeight: 1.6,
          color: descBad ? "var(--carle-red)" : "var(--ink-soft)", fontWeight: descBad ? 700 : 400 }}>
          {descLen} / {MOY_MAX_BILL_DESC} 文字{descBad ? "。長すぎます" : ""}
        </div>
      </div>

      <div style={{ marginTop: 14 }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>有効期限</div>
        <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", border: "1.5px solid var(--ink)" }}>
          {MOY_BILL_TTLS.map(([secs, label], i) => (
            <button key={secs} disabled={busy} onClick={() => setTtl(secs)} style={{
              padding: "9px 0", border: "none", borderLeft: i === 0 ? "none" : "1.5px solid var(--ink)",
              background: ttl === secs ? "var(--moy-deep)" : "var(--bg-white)",
              color: ttl === secs ? "#fff" : "var(--ink)", cursor: busy ? "default" : "pointer",
              fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>{label}</button>
          ))}
        </div>
      </div>

      <div style={{ marginTop: 14 }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 4 }}>注文番号（任意）</div>
        <input value={orderRef} disabled={busy} onChange={(e) => setOrderRef(e.target.value)}
          maxLength={MOY_MAX_BILL_REF * 3} aria-label="注文番号（任意・64文字まで）"
          placeholder="お店で使っている管理用の番号" style={fieldStyle} />
        {!refOk && (
          <div style={{ marginTop: 4, fontFamily: "var(--font-jp)", fontSize: 11, lineHeight: 1.6,
            fontWeight: 700, color: "var(--carle-red)" }}>
            いま {refLen} 文字です。{MOY_MAX_BILL_REF} 文字までにするか、空欄にしてください。
          </div>
        )}
      </div>

      {err && (
        <div role="alert" style={{ marginTop: 12, padding: "9px 12px", border: "1.5px solid var(--carle-red)",
          background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
          fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>{moyBillErrText(err)}</div>
      )}

      <button disabled={!canSend} onClick={send} style={{ ...ctaStyle(!canSend), marginTop: 14 }}>
        {busy ? "作成しています…" : "この内容で請求を作る"}
      </button>
      <button disabled={busy} onClick={onCancel} style={{ width: "100%", marginTop: 6, padding: "10px",
        border: "none", background: "transparent", cursor: busy ? "default" : "pointer",
        fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>やめる</button>
    </div>
  );
}

/* 請求の 1 行。未払いは QR の表示と取り消し、支払い済みは売上ページと同じ
   エスクロー表示（SaleEscrowNotes。渡し待ちなら「全額渡した」）。 */
function MoyBillRow({ sale, last, merchantId, onChanged, onReported }) {
  const [showQr, setShowQr] = msState(false);
  const [confirming, setConfirming] = msState(false);
  const [busy, setBusy] = msState(false);
  const [canceled, setCanceled] = msState(false);
  const [cancelErr, setCancelErr] = msState(null); // {text, stale}
  const aliveRef = msRef(true);
  msEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);

  const escrow = sale.escrow || { stage: "none" };
  const isOpen = sale.state === "created";
  const isHeld = sale.state === "paid" && escrow.stage === "held";
  const sourceLabel = MOY_BILL_SOURCE[sale.source];
  const hasQr = typeof sale.qr_url === "string" && sale.qr_url !== "";

  async function doCancel() {
    if (busy) return;
    setBusy(true); setCancelErr(null);
    let ok = false;
    let err = null;
    try {
      const r = await MoyMoy.portalIntentCancel(merchantId, sale.intent_id);
      if (r.ok) ok = true;
      else {
        // 払われていた / もう取り消せない状態 / 見つからない = 手元の行が古い。
        // 取り直すまで同じ操作を繰り返させない。
        const stale = r.error === "already_paid" || r.error === "not_cancelable" || r.error === "unknown_intent";
        err = { text: moyBillErrText(r), stale };
      }
    } catch (e) {
      console.error("MoyMoy: intent cancel failed", e);
      err = { text: errLabelFor({ code: "network" }), stale: false };
    }
    if (!aliveRef.current) return;
    setBusy(false); setConfirming(false);
    if (ok) { setCanceled(true); onChanged(); }
    else setCancelErr(err);
  }

  return (
    <div style={{ padding: "12px 14px", background: "var(--bg-white)",
      borderLeft: isHeld ? "4px solid var(--carle-red)" : "4px solid transparent",
      borderBottom: last ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
      <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 10 }}>
        <div style={{ minWidth: 0 }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700, color: "var(--ink)",
            whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
            {sale.description || "（内容の記載なし）"}
          </div>
          <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)", marginTop: 2,
            display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{moyDateLabel(sale.created_unix_ms)}</span>
            {sourceLabel && (
              <span style={{ border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
                color: "var(--moy-deep)", padding: "0 5px", fontFamily: "var(--font-jp)", fontSize: 10,
                fontWeight: 700, whiteSpace: "nowrap" }}>{sourceLabel}</span>
            )}
          </div>
          {isOpen && !canceled && (
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", marginTop: 2 }}>
              期限 {moyDateLabel(sale.expires_unix_ms)}
            </div>
          )}
          {sale.order_ref && (
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)", marginTop: 2,
              wordBreak: "break-all" }}>注文番号 {sale.order_ref}</div>
          )}
        </div>
        <div style={{ textAlign: "right", flexShrink: 0 }}>
          <div style={{ fontFamily: "var(--font-mono)", fontWeight: 700, fontSize: 15, color: "var(--ink)" }}>
            {formatEme(sale.amount_minor)} エメ
          </div>
          <div style={{ marginTop: 4, display: "flex", flexDirection: "column", alignItems: "flex-end", gap: 3 }}>
            <MoyBillStateBadge state={canceled ? "canceled" : sale.state} />
            {sale.state === "paid" && <EscrowStageBadge stage={escrow.stage} />}
          </div>
        </div>
      </div>

      {isOpen && !canceled && (
        <div style={{ marginTop: 8 }}>
          {hasQr && (
            <button onClick={() => setShowQr(!showQr)} style={{ padding: "8px 14px",
              border: "1.5px solid var(--ink)", background: "var(--bg-white)", cursor: "pointer",
              fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>
              {showQr ? "QR を隠す" : "QR を表示"}
            </button>
          )}
          {showQr && hasQr && <MoyBillQr qrUrl={sale.qr_url} description={sale.description || ""} />}

          {cancelErr && (
            <div role="alert" style={{ marginTop: 8, padding: "8px 10px", border: "1.5px solid var(--carle-red)",
              background: "rgba(227,38,54,0.08)", color: "var(--carle-red)", fontFamily: "var(--font-jp)",
              fontSize: 12, fontWeight: 700, lineHeight: 1.6 }}>
              {cancelErr.text}
              {cancelErr.stale && (
                <div>
                  <button onClick={onChanged} style={{ marginTop: 6, padding: "6px 12px",
                    border: "1.5px solid var(--carle-red)", background: "var(--bg-white)", color: "var(--carle-red)",
                    cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 12 }}>
                    一覧を更新する
                  </button>
                </div>
              )}
            </div>
          )}

          {!(cancelErr && cancelErr.stale) && !confirming && (
            <button disabled={busy} onClick={() => { setCancelErr(null); setConfirming(true); }}
              style={{ display: "block", marginTop: 8, padding: "8px 14px", border: "1.5px solid var(--carle-red)",
              background: "rgba(227,38,54,0.06)", color: "var(--carle-red)", cursor: "pointer",
              fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>取り消す</button>
          )}
          {confirming && (
            <div style={{ marginTop: 8, padding: "10px 12px", border: "1.5px solid var(--carle-red)" }}>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink)", lineHeight: 1.7 }}>
                この請求を取り消します。QR を読み取っても、もう支払えなくなります。
              </div>
              <div style={{ marginTop: 8, display: "flex", gap: 8 }}>
                <button disabled={busy} onClick={doCancel} style={{ flex: 1, padding: "10px",
                  border: "1.5px solid var(--carle-red)", background: busy ? "#e6e6e6" : "var(--carle-red)",
                  color: busy ? "var(--ink-soft)" : "#fff", cursor: busy ? "default" : "pointer",
                  fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 13 }}>
                  {busy ? "取り消しています…" : "取り消す"}
                </button>
                <button disabled={busy} onClick={() => setConfirming(false)} style={{ flex: 1, padding: "10px",
                  border: "1.5px solid var(--ink)", background: "var(--bg-white)", cursor: busy ? "default" : "pointer",
                  fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>やめる</button>
              </div>
            </div>
          )}
        </div>
      )}
      {canceled && (
        <div role="status" style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
          color: "var(--moy-deep)" }}>取り消しました。</div>
      )}

      {sale.state === "paid" && (
        <SaleEscrowNotes sale={sale} merchantId={merchantId} onReported={onReported} />
      )}
    </div>
  );
}

function MoyBillsPage({ merchant }) {
  const merchantId = merchant.merchant_id;
  const active = merchant.status === "active";
  const [filter, setFilter] = msState("held");
  const [data, setData] = msState(null);        // /sales の応答 ({ok:true,...} | {ok:false,error})
  const [loading, setLoading] = msState(true);
  const [banner, setBanner] = msState(null);    // 取り直しの失敗 {code}
  const [notice, setNotice] = msState(null);    // 渡し待ちの行を報告した結果 {ok, text}
  const [formOpen, setFormOpen] = msState(false);
  const [created, setCreated] = msState(null);  // 作成結果カード
  // 取得は重なりうる（チップの連打、報告直後の取り直し）。遅れて返った古い応答で
  // 一覧を上書きしないよう、最後に始めた取得の応答だけを採用する。
  const seqRef = msRef(0);
  const filterRef = msRef("held");
  filterRef.current = filter;
  const aliveRef = msRef(true);
  msEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);

  // quiet = 一覧を消さずに取り直す。報告・取り消しのあとに行の結果表示を残すため。
  async function load(state, quiet) {
    const seq = ++seqRef.current;
    if (!quiet) { setLoading(true); setData(null); }
    let answer;
    try {
      answer = await MoyMoy.merchantSales(merchantId, undefined, state);
    } catch (e) {
      console.warn("MoyMoy: merchant bills load failed", e);
      answer = { ok: false, error: "network" };
    }
    if (!aliveRef.current || seq !== seqRef.current) return;
    setLoading(false);
    if (answer.ok) { setData(answer); setBanner(null); }
    else if (quiet) setBanner({ code: answer.error || "error" });
    else setData(answer);
  }
  msEffect(() => {
    setNotice(null); setBanner(null);
    load(filter, false);
  }, [filter, merchantId]);
  const refresh = () => load(filterRef.current, true);

  function handleCreated(r, description) {
    setCreated({ ...r, description });
    setFormOpen(false);
    // 作った請求が見える「未払い」に切り替える。すでに未払いなら取り直すだけ。
    if (filterRef.current === "open") refresh();
    else setFilter("open");
  }

  // 渡し待ちの絞り込み中に報告すると、その行は取り直しで一覧から消え、行の結果表示も
  // 一緒に消える。何を報告したかが分かるよう、結果を一覧の上に残す。
  function handleReported(sale, res) {
    if (res && filterRef.current === "held") {
      setNotice({ ok: res.ok, text: "「" + (sale.description || "請求") + "」 " + res.text });
    }
    refresh();
  }

  const spec = MOY_BILL_FILTERS.find((f) => f.key === filter);
  const rows = data && data.ok ? (data.sales || []) : [];
  return (
    <>
      <div style={{ fontFamily: "var(--font-jp)", fontSize: 18, fontWeight: 800, color: "var(--ink)" }}>
        {merchant.name} の請求
      </div>
      <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
        お客さまが払うと、代金はいったん MoyMoy が預かります。商品を渡したら「全額渡した」で報告してください。
      </div>
      <div style={{ marginTop: 8, padding: "9px 12px", border: "1.5px solid var(--carle-red)",
        background: "rgba(227,38,54,0.06)", fontFamily: "var(--font-jp)", fontSize: 12,
        fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>
        報告しないまま 6 時間経つと凍結され、お店には入金されません。
      </div>

      {banner && (
        <div role="alert" style={{ marginTop: 12, padding: "10px 12px", border: "1.5px solid var(--carle-red)",
          background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
          fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.6 }}>{moyBillErrText({ error: banner.code })}</div>
      )}

      {created && (
        <div style={{ marginTop: 14, border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)", padding: "14px" }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 800, color: "var(--moy-deep)" }}>
            請求を作成しました
          </div>
          <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink)", lineHeight: 1.8 }}>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 10 }}>
              <span>金額</span>
              <span style={{ fontFamily: "var(--font-mono)", fontWeight: 700 }}>{formatEme(created.amount_minor)} エメ</span>
            </div>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 10 }}>
              <span>内容</span>
              <span style={{ fontWeight: 700, textAlign: "right", wordBreak: "break-all" }}>{created.description}</span>
            </div>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 10 }}>
              <span>期限</span>
              <span style={{ fontWeight: 700 }}>{moyDateLabel(created.expires_unix_ms)} まで</span>
            </div>
          </div>
          <MoyBillQr qrUrl={created.qr_url} description={created.description} />
          <button onClick={() => setCreated(null)} style={{ width: "100%", marginTop: 12, padding: "10px",
            border: "1.5px solid var(--ink)", background: "var(--bg-white)", cursor: "pointer",
            fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 13 }}>閉じる</button>
        </div>
      )}

      {!formOpen && (
        <button disabled={!active} onClick={() => { setCreated(null); setFormOpen(true); }} style={{ width: "100%",
          marginTop: 14, padding: "14px", border: "1.5px solid var(--ink)",
          background: active ? "var(--bg-white)" : "#e6e6e6", boxShadow: active ? "3px 3px 0 var(--ink)" : "none",
          cursor: active ? "pointer" : "default", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15 }}>
          ＋ 新しい請求
        </button>
      )}
      {!formOpen && !active && (
        <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", lineHeight: 1.7 }}>
          稼働中のお店だけ請求を作成できます。
        </div>
      )}
      {formOpen && (
        <MoyNewBillForm merchantId={merchantId} onCreated={handleCreated} onCancel={() => setFormOpen(false)} />
      )}

      <div style={{ marginTop: 18, display: "flex", flexWrap: "wrap", gap: 6 }}>
        {MOY_BILL_FILTERS.map((f) => (
          <button key={f.key} onClick={() => setFilter(f.key)} aria-pressed={filter === f.key} style={{
            padding: "7px 12px", border: "1.5px solid " + (filter === f.key ? "var(--moy-deep)" : "var(--ink)"),
            background: filter === f.key ? "var(--moy-deep)" : "var(--bg-white)",
            color: filter === f.key ? "#fff" : "var(--ink)", cursor: "pointer",
            fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 12 }}>{f.label}</button>
        ))}
      </div>

      {notice && (
        <div role={notice.ok ? "status" : "alert"} style={{ marginTop: 10, padding: "8px 10px",
          border: "1.5px solid " + (notice.ok ? "var(--moy-deep)" : "var(--carle-red)"),
          background: notice.ok ? "var(--moy-mint)" : "rgba(227,38,54,0.08)",
          color: notice.ok ? "var(--moy-deep)" : "var(--carle-red)",
          fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, lineHeight: 1.6 }}>
          {notice.text}
          <div>
            <button onClick={() => setNotice(null)} style={{ marginTop: 4, padding: "2px 0", border: "none",
              background: "transparent", cursor: "pointer", fontFamily: "var(--font-jp)", fontSize: 11,
              fontWeight: 600, color: "inherit", textDecoration: "underline" }}>閉じる</button>
          </div>
        </div>
      )}

      {loading && (
        <div style={{ padding: "40px 0", textAlign: "center", fontFamily: "var(--font-jp)",
          fontSize: 13, color: "var(--ink-soft)" }}>読み込み中…</div>
      )}
      {!loading && data && !data.ok && (
        <div style={{ padding: "20px 0", textAlign: "center", fontFamily: "var(--font-jp)",
          fontSize: 13, fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>
          {moyBillErrText(data)}
        </div>
      )}
      {!loading && data && data.ok && (
        <>
          <div style={{ marginTop: 10, border: "1.5px solid var(--ink)" }}>
            {rows.map((s, i) => (
              <MoyBillRow key={s.intent_id} sale={s} last={i === rows.length - 1} merchantId={merchantId}
                onChanged={refresh} onReported={(res) => handleReported(s, res)} />
            ))}
            {rows.length === 0 && (
              <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
                fontSize: 13, color: "var(--ink-soft)" }}>{spec ? spec.empty : "請求はありません"}</div>
            )}
          </div>
          {data.truncated && (
            <div style={{ marginTop: 10, padding: "9px 12px", border: "1.5px solid var(--ink)",
              background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-jp)", fontSize: 11,
              fontWeight: 700, color: "var(--ink)", lineHeight: 1.6 }}>
              さらに古い請求があります（最新 {formatCount(data.limit)} 件を表示）
            </div>
          )}
        </>
      )}
    </>
  );
}

/* inline: ボトムナビの「お店」タブとして本体レイアウトに埋め込む形。アプリの
   ヘッダとボトムナビが既にあるので、自前の全画面ヘッダと「閉じる」を出さず、
   一覧以外の画面でだけ戻るボタンを出す。PIN シート (MoyPinStep) はこの要素の
   外側（アプリ本体）を基準に全画面で出る。 */
function MerchantPortal({ open, onClose, inline }) {
  const [list, setList] = msState([]);
  const [loading, setLoading] = msState(true);
  const [loadErr, setLoadErr] = msState(null);
  const [view, setView] = msState("list");   // list | register | detail | sales | bills
  const [selected, setSelected] = msState(null); // merchant_id
  const [revealed, setRevealed] = msState(null); // 一度だけ見せる平文キー
  const [ask, setAsk] = msState(null);       // {title, sub, cta, run(pin)}
  const [askErr, setAskErr] = msState(null);
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState(null);
  const [form, setForm] = msState({ name: "", sub: "" });
  const [limits, setLimits] = msState({ max_open_intents: "", daily_issue_cap: "" });
  // 売上 (エスクロー状況)。/merchant/portal/sales の応答をそのまま持つ
  // ({ok:true, ...} または {ok:false, error}) — 未取得なら null。
  const [sales, setSales] = msState(null);
  const [salesLoading, setSalesLoading] = msState(false);
  // 店ごとの「渡し待ち」件数 { [merchant_id]: {n, more} }。一覧と詳細の目立つ表示用。
  const [heldMap, setHeldMap] = msState({});
  const aliveRef = msRef(true);
  // 非同期の売上取り直しが返ってきたとき、まだ同じ店の売上ページを見ているかの照合用。
  const selectedRef = msRef(null);
  selectedRef.current = selected;

  // 履行を報告したあと、行を消さずに売上だけ取り直す。読み込み表示や
  // `setSales(null)` を挟むと一覧全体が一度消えて、報告した行の結果表示も
  // 見えなくなる。失敗したときは一覧を差し替えず、上のバナーに出す。
  async function refreshSales() {
    const id = selectedRef.current;
    if (!id) return;
    try {
      const r = await MoyMoy.merchantSales(id);
      if (!aliveRef.current || selectedRef.current !== id) return;
      if (r.ok) setSales(r);
      else setErr({ code: r.error || "error" });
    } catch (e) {
      console.error("MoyMoy: merchant sales refresh failed", e);
      if (aliveRef.current) setErr({ code: "network" });
    }
  }

  // 各店の渡し待ち件数 (state=held の行数)。取れなかった店は {failed:true} にして、
  // 「0 件」と区別して画面に出す（ページ全体のエラーにはしない）。
  // 上限で切れていたら `more` を立て、画面では「N 件以上」と言う。
  async function loadHeld(merchants) {
    await Promise.all(merchants.map(async (m) => {
      try {
        const r = await MoyMoy.merchantSales(m.merchant_id, undefined, "held");
        if (!aliveRef.current) return;
        if (r.ok) {
          setHeldMap((prev) => ({ ...prev, [m.merchant_id]: { n: (r.sales || []).length, more: !!r.truncated } }));
        } else {
          console.warn("MoyMoy: held count refused for " + m.merchant_id, r.error);
          setHeldMap((prev) => ({ ...prev, [m.merchant_id]: { failed: true } }));
        }
      } catch (e) {
        console.warn("MoyMoy: held count failed for " + m.merchant_id, e);
        if (aliveRef.current) setHeldMap((prev) => ({ ...prev, [m.merchant_id]: { failed: true } }));
      }
    }));
  }

  async function reload() {
    try {
      const r = await MoyMoy.merchantList();
      if (!aliveRef.current) return;
      if (r.ok) { setList(r.merchants || []); setLoadErr(null); loadHeld(r.merchants || []); }
      else setLoadErr(r.error || "error");
    } catch (e) {
      console.warn("MoyMoy: merchant portal list failed", e);
      if (aliveRef.current) setLoadErr("network");
    }
    if (aliveRef.current) setLoading(false);
  }

  msEffect(() => {
    if (!open) return;
    aliveRef.current = true;
    setLoading(true); setView("list"); setErr(null); setRevealed(null); setAsk(null);
    setSales(null); setSalesLoading(false); setHeldMap({});
    reload();
    return () => { aliveRef.current = false; };
  }, [open]);

  // 売上タブを開いたとき（または別の加盟店に切り替えたとき）に読み直す。
  // list/register/detail では叩かない — 見てもいない画面のために毎回
  // 往復させない。
  msEffect(() => {
    if (view !== "sales" || !selected) return;
    let alive = true;
    (async () => {
      setSalesLoading(true); setSales(null);
      try {
        const r = await MoyMoy.merchantSales(selected);
        if (alive) setSales(r);
      } catch (e) {
        console.warn("MoyMoy: merchant sales load failed", e);
        if (alive) setSales({ ok: false, error: "network" });
      }
      if (alive) setSalesLoading(false);
    })();
    return () => { alive = false; };
  }, [view, selected]);

  if (!open) return null;

  const current = list.find((m) => m.merchant_id === selected) || null;

  // PIN を集めてから実行する操作の共通ラッパ。PIN 由来の失敗だけシートに
  // 留めて再入力させ、それ以外は本体側にエラーを出して閉じる。
  function askPin(spec) { setAskErr(null); setAsk(spec); }
  async function runAsk(pin) {
    if (!ask || busy) return;
    setBusy(true); setAskErr(null);
    try {
      const r = await ask.run(pin);
      if (!aliveRef.current) return;
      if (r && r.ok) {
        setAsk(null); setErr(null);
        await reload();
      } else if (r && (r.error === "invalid_pin" || r.error === "locked" || r.error === "too_many_attempts")) {
        setAskErr({ code: r.error, retry_after_ms: r.retry_after_ms });
      } else {
        setAsk(null);
        setErr({ code: (r && r.error) || "error", reason: r && r.reason });
      }
    } catch (e) {
      console.warn("MoyMoy: merchant portal action failed", e);
      if (aliveRef.current) setAskErr({ code: "network" });
    }
    if (aliveRef.current) setBusy(false);
  }

  function doRegister() {
    const name = form.name.trim();
    const sub = form.sub.trim();
    askPin({
      title: "PIN で確認", sub: "加盟店を登録します", cta: "登録する",
      run: async (pin) => {
        const r = await MoyMoy.merchantRegister({ name, sub: sub || undefined, pin });
        if (r.ok) {
          setRevealed({ merchant_id: r.merchant_id, api_key: r.api_key });
          setSelected(r.merchant_id);
          setView("detail");
          setForm({ name: "", sub: "" });
        }
        return r;
      },
    });
  }

  function doRotate(m) {
    askPin({
      title: "PIN で確認", sub: m.name + " のキーを再発行", cta: "再発行する",
      run: async (pin) => {
        const r = await MoyMoy.merchantRotateKey(m.merchant_id, pin);
        if (r.ok) setRevealed({ merchant_id: m.merchant_id, api_key: r.api_key });
        return r;
      },
    });
  }

  // 発行 / 作り直し。作り直すと古い QR は無効になり、すでに店頭に設定した
  // ものも使えなくなる — PIN を求める前に、その場で何が起きるかを書く。
  // 成功したら runAsk が一覧を取り直すので、新しい URL はそこから表示される。
  function doStaticQr(m, reissue) {
    askPin({
      title: "PIN で確認",
      sub: reissue
        ? m.name + " の店頭 QR を作り直します（古い QR は使えなくなります）"
        : m.name + " の店頭 QR を発行します",
      cta: reissue ? "作り直す" : "発行する",
      run: (pin) => MoyMoy.portalStaticQr(m.merchant_id, pin),
    });
  }

  function doStatus(m, status) {
    askPin({
      title: "PIN で確認",
      sub: m.name + (status === "disabled" ? " を停止" : " を再開"),
      cta: status === "disabled" ? "停止する" : "再開する",
      run: (pin) => MoyMoy.merchantSetStatus(m.merchant_id, status, pin),
    });
  }

  function doLimits(m) {
    const open = parseInt(limits.max_open_intents, 10);
    // この入力欄は（プレースホルダ・案内文と同じく）整数エメで打たせる。
    // daily_issue_cap はサーバ側では minor なので、送信直前にだけ ×100 する
    // — ここを忘れると、加盟店が意図した上限の 1/100 がサーバへ通ってしまう。
    const capEme = parseInt(limits.daily_issue_cap, 10);
    askPin({
      title: "PIN で確認", sub: m.name + " の発行上限を変更", cta: "変更する",
      run: async (pin) => {
        const r = await MoyMoy.merchantSetLimits(m.merchant_id, pin, {
          max_open_intents: Number.isFinite(open) ? open : undefined,
          daily_issue_cap: Number.isFinite(capEme) ? capEme * 100 : undefined,
        });
        if (r.ok) setLimits({ max_open_intents: "", daily_issue_cap: "" });
        return r;
      },
    });
  }

  // 送信可否はサーバと同じ数え方で決める（moyTextLen のコメント参照）。
  const nameLen = moyTextLen(form.name);
  const nameOk = nameLen >= 1 && nameLen <= 32;
  // 「何か打ってあるのに送れない」状態。未入力での無効はボタンを見れば分かるので、
  // 赤字を出すのはこちらのときだけ — 空欄の初期状態で警告を出さない。
  const nameBad = form.name.trim() !== "" && !nameOk;
  // sub は任意。trim 後が空なら「未指定」で、doRegister が undefined を送り、
  // サーバも `sub_in.map(str::trim).filter(|s| !s.is_empty())` で None にするので
  // エラーにならない。中身があるときだけ 48 スカラーの上限が掛かる。
  // 下限 1 を残すのは、trim 後は空でなくても NFKC 後の trim で空になれば
  // サーバが bad_sub / empty を返すため — その場合も送らせない。
  const subTrimmed = form.sub.trim();
  const subLen = subTrimmed ? moyTextLen(subTrimmed) : 0;
  const subOk = subTrimmed === "" || (subLen >= 1 && subLen <= 48);
  // 長すぎる sub は今まで nameOk に入っておらず、送信されてサーバに bad_sub で
  // 弾かれていた。PIN を打たせてから失敗させることになるので、ここで止める。
  const canRegister = nameOk && subOk;

  // 店を選んで開く。上限の入力欄は店ごとなので、持ち越すと別の店の値を送りかねない
  // — 開くたびに空に戻す。
  function openShop(m, nextView) {
    setSelected(m.merchant_id); setRevealed(null); setErr(null);
    setLimits({ max_open_intents: "", daily_issue_cap: "" });
    setView(nextView);
  }

  // 一つ上の画面へ戻る。一覧ではオーバーレイ形のときだけ閉じる。
  function goBack() {
    if (view === "list") { if (onClose) onClose(); return; }
    // 売上・請求はある加盟店の詳細のサブ画面なので、一覧ではなく詳細へ戻る。
    // 報告や取り消しで渡し待ちの件数が変わっているので、戻るときに数え直す。
    if (view === "sales" || view === "bills") { setView("detail"); setErr(null); loadHeld(list); return; }
    setView("list"); setErr(null); setRevealed(null);
  }

  return (
    <>
      <div className="fade-in" style={inline
        ? { flex: 1, minHeight: 0, position: "relative", background: "var(--bg-white)",
            display: "flex", flexDirection: "column" }
        : { position: "absolute", inset: 0, zIndex: 145,
            background: "var(--bg-white)", display: "flex", flexDirection: "column" }}>
        {inline ? (
          <div style={{ flexShrink: 0, padding: "14px 18px 10px", borderBottom: "1px solid rgba(0,0,0,0.08)",
            display: "flex", alignItems: "center", gap: 10 }}>
            {view !== "list" && (
              <button onClick={goBack} aria-label={view === "sales" || view === "bills" ? "詳細へ戻る" : "一覧へ戻る"}
                style={{ background: "transparent", border: "1.5px solid var(--ink)", cursor: "pointer",
                color: "var(--ink)", padding: "4px 8px", display: "flex", alignItems: "center", gap: 4,
                fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 }}>
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>
                {view === "sales" || view === "bills" ? "詳細へ" : "一覧へ"}
              </button>
            )}
            <div>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 16, fontWeight: 800, color: "var(--ink)" }}>お店</div>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)" }}>
                加盟店管理 · MoyMoy で代金を受け取る
              </div>
            </div>
          </div>
        ) : (
        <div style={{ flexShrink: 0, padding: "56px 18px 14px", background: "var(--moy)", color: "#fff",
          borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
          <div className="paper-noise" style={{ opacity: 0.1 }} />
          <div style={{ position: "relative", zIndex: 1 }}>
            <button
              onClick={goBack}
              style={{ background: "transparent", border: "none", cursor: "pointer", color: "#fff",
              fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700, letterSpacing: "0.16em",
              display: "flex", alignItems: "center", gap: 6, marginBottom: 8 }}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>
              {view === "list" ? "閉じる" : view === "sales" || view === "bills" ? "詳細へ" : "一覧へ"}
            </button>
            <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <EmeGem size={24} />
              <div>
                <div style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 20,
                  letterSpacing: "-0.02em" }}>加盟店管理</div>
                <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, opacity: 0.9 }}>
                  MoyMoy で代金を受け取る
                </div>
              </div>
            </div>
          </div>
        </div>
        )}

        <div style={{ flex: 1, overflow: "auto", padding: inline ? "16px 18px 120px" : "16px 18px 30px" }}>
          {err && (
            <div style={{ marginBottom: 12, padding: "10px 12px", border: "1.5px solid var(--carle-red)",
              background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
              fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.6 }}>
              {errLabelFor(err)}
              {err.reason ? "（" + (REASON_LABEL[err.reason] || err.reason) + "）" : ""}
            </div>
          )}

          {view === "list" && (
            <>
              {loading && (
                <div style={{ padding: "40px 0", textAlign: "center", fontFamily: "var(--font-jp)",
                  fontSize: 13, color: "var(--ink-soft)" }}>読み込み中…</div>
              )}
              {!loading && loadErr && (
                <div style={{ padding: "20px 0", textAlign: "center", fontFamily: "var(--font-jp)",
                  fontSize: 13, fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>
                  {errLabelFor({ code: loadErr })}
                </div>
              )}
              {!loading && !loadErr && (
                <>
                  <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)",
                    lineHeight: 1.8 }}>
                    加盟店を登録すると、あなたのアプリやお店から MoyMoy 決済を呼び出せます。
                    売上はこの口座にそのまま入ります。
                  </div>
                  <div style={{ marginTop: 14, border: "1.5px solid var(--ink)" }}>
                    {list.map((m, i) => (
                      <div key={m.merchant_id} style={{ background: "var(--bg-white)",
                        borderBottom: i === list.length - 1 ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
                      <button
                        onClick={() => openShop(m, "detail")}
                        style={{ width: "100%", textAlign: "left", cursor: "pointer", display: "flex",
                        alignItems: "center", gap: 12, padding: "12px 14px", background: "var(--bg-white)",
                        border: "none" }}>
                        <div style={{ width: 38, height: 38, flexShrink: 0 }}>
                          <CrystalIcon palette={m.status === "active" ? "emerald" : "orange"} glyph="◈" size={38} />
                        </div>
                        <div style={{ flex: 1, minWidth: 0 }}>
                          <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700,
                            color: "var(--ink)", whiteSpace: "nowrap", overflow: "hidden",
                            textOverflow: "ellipsis" }}>{m.name}</div>
                          <div style={{ fontFamily: "var(--font-mono)", fontSize: 10,
                            color: "var(--ink-soft)", marginTop: 1 }}>
                            {MOY_STATUS_LABEL[m.status] || m.status} · {m.api_key_prefix || "キー未発行"}
                          </div>
                        </div>
                        <span style={{ fontFamily: "var(--font-mono)", fontSize: 18, color: "var(--moy-deep)" }}>›</span>
                      </button>
                      {/* 渡し待ちがあるお店だけ。ボタンの中にボタンは置けないので兄弟として並べる。 */}
                      {heldMap[m.merchant_id] && (heldMap[m.merchant_id].n > 0 || heldMap[m.merchant_id].failed) && (
                        <div style={{ padding: "0 14px 12px" }}>
                          <MoyHeldAlert held={heldMap[m.merchant_id]} onClick={() => openShop(m, "bills")} />
                        </div>
                      )}
                      </div>
                    ))}
                    {list.length === 0 && (
                      <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
                        fontSize: 13, color: "var(--ink-soft)" }}>まだ加盟店がありません</div>
                    )}
                  </div>
                  <button onClick={() => { setErr(null); setView("register"); }} style={{ width: "100%",
                    marginTop: 14, padding: "14px", border: "1.5px solid var(--ink)",
                    background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)", cursor: "pointer",
                    fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15 }}>
                    ＋ 加盟店を登録する
                  </button>
                </>
              )}
            </>
          )}

          {view === "register" && (
            <>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)",
                lineHeight: 1.8 }}>
                店名はあとから変更できません。お客さまの承認画面に出るのは、あなたの
                @ID と、ここで決めた店名です。
              </div>
              {/* maxLength は UTF-16 コード単位でしか数えられないので、本当の上限
                  （NFKC 後のスカラー 32 / 48）を表現できない。32 のままだと絵文字の
                  店名が 16 個で打ち止めになり、正当な名前を物理的に入力できない。
                  外さずに残すのは、数 KB の貼り付けがそのまま state に入るのを
                  止める暴走防止としてだけ。上限の 8 倍にしてあるので、
                  スカラー 32 / 48 に届く前にこの値が効くことはない。判定は
                  nameOk / subOk が持つ。 */}
              <div style={{ marginTop: 16 }}>
                <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>店名（変更不可）</div>
                <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })}
                  maxLength={256} aria-label="店名（登録後は変更できません）"
                  placeholder="ピグルショップ" style={fieldStyle} />
                {/* maxLength を緩めた分、長すぎる名前は「打てるのにボタンだけ黙って
                    無効」になりうる。既存の注記を赤にして理由を出す。文字数を出すのは、
                    画面に見えている文字数と数え方（NFKC 後）が食い違うため。 */}
                <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, marginTop: 6, lineHeight: 1.7,
                  color: nameBad ? "var(--carle-red)" : "var(--ink-soft)",
                  fontWeight: nameBad ? 700 : 400 }}>
                  {nameBad
                    ? "いま " + nameLen + " 文字です（㈱ などの記号は変換後の文字数で数えます）。32文字までにしてください。"
                    : "32文字まで。運営を騙る語や、複数の文字体系を混ぜた紛らわしい名前は登録できません。"}
                </div>
                {/* 予約語とスクリプト混在の条件は、長さの警告が出ているあいだも
                    消さない。上の行を差し替えるだけだと、名前を削って長さを直した
                    利用者が、次に何で弾かれるかを知らないまま送信することになる。 */}
                {nameBad && (
                  <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, marginTop: 4, lineHeight: 1.7,
                    color: "var(--ink-soft)" }}>
                    運営を騙る語や、複数の文字体系を混ぜた紛らわしい名前は登録できません。
                  </div>
                )}
              </div>
              <div style={{ marginTop: 16 }}>
                <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>ひとこと説明（任意）</div>
                <input value={form.sub} onChange={(e) => setForm({ ...form, sub: e.target.value })}
                  maxLength={384} aria-label="お店のひとこと説明（任意）"
                  placeholder="建材と装飾ブロックのお店" style={fieldStyle} />
                {/* この欄には元々注記が無い。上限に触れたときだけ出す — 任意欄に
                    常時「48文字まで」を置くと、店名の注記と競って読まれなくなる。 */}
                {!subOk && (
                  <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, marginTop: 6, lineHeight: 1.7,
                    fontWeight: 700, color: "var(--carle-red)" }}>
                    いま {subLen} 文字です。48文字までにするか、空欄にしてください。
                  </div>
                )}
              </div>
              <button disabled={!canRegister} onClick={doRegister} style={{ ...ctaStyle(!canRegister), marginTop: 20 }}>
                <EmeGem size={20} /> PIN で確認して登録
              </button>
            </>
          )}

          {view === "detail" && current && (
            <>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 18, fontWeight: 800, color: "var(--ink)" }}>
                {current.name}
              </div>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)", marginTop: 2 }}>
                {current.merchant_id}
              </div>

              {revealed && revealed.merchant_id === current.merchant_id && (
                <MoyApiKeyReveal apiKey={revealed.api_key} onClose={() => setRevealed(null)} />
              )}

              {/* 渡し待ち（支払い済みで、まだ「渡した」を報告していない請求）。最優先の
                  作業なので、他のボタンより上に赤で出す。 */}
              <div style={{ marginTop: 8 }}>
                <MoyHeldAlert held={heldMap[current.merchant_id]}
                  onClick={() => { setErr(null); setView("bills"); }} />
              </div>

              <button onClick={() => { setErr(null); setView("bills"); }} style={{ width: "100%", marginTop: 16,
                padding: "14px", border: "1.5px solid var(--ink)", background: "var(--bg-white)",
                boxShadow: "3px 3px 0 var(--ink)", cursor: "pointer", fontFamily: "var(--font-jp)",
                fontWeight: 700, fontSize: 15 }}>
                請求を管理する（作成・取消・渡した報告）
              </button>

              <button onClick={() => { setErr(null); setView("sales"); }} style={{ width: "100%", marginTop: 10,
                padding: "14px", border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
                cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15,
                color: "var(--moy-deep)", display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
                <EmeGem size={18} /> 売上を見る（保留中の入金を確認）
              </button>

              <div style={{ marginTop: 16, border: "1.5px solid var(--ink)", padding: "12px 14px",
                fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)" }}>
                {[
                  ["状態", MOY_STATUS_LABEL[current.status] || current.status],
                  ["一覧掲載", current.listed ? "あり" : "なし"],
                  ["APIキー", current.api_key_prefix ? current.api_key_prefix + "…" : "未発行"],
                  ["キー発行日", moyDateLabel(current.api_key_created_unix_ms)],
                  // 最終利用時刻は漏洩の唯一の手がかりになる。今日取引していない
                  // のに今日の時刻が出ていれば、キーが誰かの手にあると分かる。
                  ["キー最終利用", moyDateLabel(current.api_key_last_used_unix_ms)],
                  ["未決済の上限", current.max_open_intents + " 件"],
                  ["24時間の発行上限", formatEme(current.daily_issue_cap) + " エメ"],
                  ["登録日", moyDateLabel(current.created_unix_ms)],
                ].map(([k, v], i) => (
                  <div key={k} style={{ display: "flex", justifyContent: "space-between", gap: 10,
                    padding: "5px 0", borderTop: i === 0 ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
                    <span>{k}</span>
                    <span style={{ color: "var(--ink)", fontWeight: 700, textAlign: "right" }}>{v}</span>
                  </div>
                ))}
              </div>
              <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11,
                color: "var(--ink-soft)", lineHeight: 1.7 }}>
                「キー最終利用」に心当たりのない時刻が出ていたら、キーが漏れています。
                すぐに停止するか、キーを再発行してください。
              </div>

              <button onClick={() => doRotate(current)} style={{ width: "100%", marginTop: 16,
                padding: "14px", border: "1.5px solid var(--ink)", background: "var(--bg-white)",
                boxShadow: "3px 3px 0 var(--ink)", cursor: "pointer", fontFamily: "var(--font-jp)",
                fontWeight: 700, fontSize: 15 }}>
                APIキーを再発行する
              </button>
              {current.status === "active" ? (
                <button onClick={() => doStatus(current, "disabled")} style={{ width: "100%", marginTop: 10,
                  padding: "14px", border: "1.5px solid var(--carle-red)", background: "rgba(227,38,54,0.06)",
                  cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15,
                  color: "var(--carle-red)" }}>
                  このお店を停止する（集金を止める）
                </button>
              ) : current.status === "disabled" ? (
                <button onClick={() => doStatus(current, "active")} style={{ width: "100%", marginTop: 10,
                  padding: "14px", border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
                  cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15,
                  color: "var(--moy-deep)" }}>
                  このお店を再開する
                </button>
              ) : null}

              <MoyStaticQrSection merchant={current} onIssue={doStaticQr} />

              <div style={{ marginTop: 22 }}>
                <div className="h-section">発行上限の変更</div>
                <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 11,
                  color: "var(--ink-soft)", lineHeight: 1.7 }}>
                  {/* max_open_intents は件数 (単位なし) であって金額ではないので
                      formatEme を通さない — 通すと minor 扱いされ「5.00」になる。
                      daily_issue_cap は金額 (minor) なので formatEme を通す。 */}
                  空欄の項目は変えません。上限は最大 500 件 /
                  {" " + formatEme(200000000)} エメまでで、それを超える値は自動的に丸められます。
                </div>
                <div style={{ marginTop: 10, display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10 }}>
                  <div>
                    <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>未決済の上限</div>
                    <input value={limits.max_open_intents} inputMode="numeric"
                      aria-label="未決済の上限（件数。空欄なら変更しません）"
                      onChange={(e) => setLimits({ ...limits, max_open_intents: e.target.value.replace(/[^0-9]/g, "") })}
                      placeholder={String(current.max_open_intents)} style={fieldStyle} />
                  </div>
                  <div>
                    <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>24時間の発行上限</div>
                    {/* current.daily_issue_cap は minor。この欄はエメで打たせるので、
                        プレースホルダも ÷100 してエメで見せる — 生の minor を出すと
                        「変更しない」つもりで打ち直した値が100倍になる。 */}
                    <input value={limits.daily_issue_cap} inputMode="numeric"
                      aria-label="24時間の発行上限（エメ。空欄なら変更しません）"
                      onChange={(e) => setLimits({ ...limits, daily_issue_cap: e.target.value.replace(/[^0-9]/g, "") })}
                      placeholder={String(Math.floor(current.daily_issue_cap / 100))} style={fieldStyle} />
                  </div>
                </div>
                <button
                  disabled={!limits.max_open_intents && !limits.daily_issue_cap}
                  onClick={() => doLimits(current)}
                  style={{ ...ctaStyle(!limits.max_open_intents && !limits.daily_issue_cap), marginTop: 12 }}>
                  PIN で確認して変更
                </button>
              </div>
            </>
          )}

          {view === "detail" && !current && (
            <div style={{ padding: "30px 0", textAlign: "center", fontFamily: "var(--font-jp)",
              fontSize: 13, color: "var(--ink-soft)" }}>加盟店が見つかりません</div>
          )}

          {view === "sales" && current && (
            <>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 18, fontWeight: 800, color: "var(--ink)" }}>
                {current.name} の売上
              </div>

              {salesLoading && (
                <div style={{ padding: "40px 0", textAlign: "center", fontFamily: "var(--font-jp)",
                  fontSize: 13, color: "var(--ink-soft)" }}>読み込み中…</div>
              )}

              {!salesLoading && sales && !sales.ok && (
                <div style={{ padding: "20px 0", textAlign: "center", fontFamily: "var(--font-jp)",
                  fontSize: 13, fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>
                  {errLabelFor({ code: sales.error })}
                </div>
              )}

              {!salesLoading && sales && sales.ok && (
                <>
                  {/* 「買い手は払ったのに残高が増えていない」への答え。この画面で
                      いちばん目立たせるべき数字。 */}
                  <div style={{ marginTop: 14, border: "1.5px solid " + (sales.held_count > 0 ? "var(--carle-red)" : "var(--ink)"),
                    background: sales.held_count > 0 ? "rgba(227,38,54,0.06)" : "var(--bg-white)",
                    padding: "14px" }}>
                    <div className="eyebrow" style={{ color: sales.held_count > 0 ? "var(--carle-red)" : "var(--moy-deep)" }}>
                      保留中（エスクロー中）の売上
                    </div>
                    <div style={{ marginTop: 4, fontFamily: "var(--font-mono)", fontWeight: 800, fontSize: 26,
                      color: sales.held_count > 0 ? "var(--carle-red)" : "var(--ink)" }}>
                      {formatEme(sales.held_total_minor)} エメ
                    </div>
                    <div style={{ marginTop: 2, fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)" }}>
                      {sales.held_count > 0
                        ? formatCount(sales.held_count) + " 件が買い手から支払い済みで、まだこの口座には入金されていません。"
                        : "現在、保留中の売上はありません。"}
                    </div>
                    {/* この合計には parked（保留のまま停止）も含まれる — 金は
                        実際にエスクローにあるため。合計だけでは「全部いつもどおり
                        待っているだけ」に見えてしまうので、一部が停止しているなら
                        ここで明示して一覧の該当行へ注意を向ける。 */}
                    {(sales.sales || []).some((s) => s.escrow && s.escrow.stage === "parked") && (
                      <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
                        color: "var(--carle-red)" }}>
                        このうち一部は保留のまま停止しています。下の一覧で「入金停止中（要確認）」の行をご確認ください。
                      </div>
                    )}
                  </div>

                  {/* 履行の報告は、この画面（「渡した」）とお店の連携（API キー）の両方
                      からできる。報告を忘れると 6 時間で凍結されて入金されない、という
                      期限は店頭 QR の運用で特に踏みやすいので、ここで明示する。 */}
                  <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 11,
                    color: "var(--ink-soft)", lineHeight: 1.7 }}>
                    商品を渡したら、下の取引の「全額渡した」を押して報告してください（一部だけの場合は「一部だけ渡した…」から）。
                    お店の連携（API キー）から自動で報告している場合は、この操作は要りません。
                    報告後、10分ほどでこの口座へ入金されます。
                  </div>
                  <div style={{ marginTop: 8, padding: "9px 12px", border: "1.5px solid var(--carle-red)",
                    background: "rgba(227,38,54,0.06)", fontFamily: "var(--font-jp)", fontSize: 12,
                    fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.7 }}>
                    報告しないまま 6 時間経つと凍結され、お店には入金されません。
                  </div>

                  {sales.truncated && (
                    <div style={{ marginTop: 14, padding: "9px 12px", border: "1.5px solid var(--ink)",
                      background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-jp)", fontSize: 11,
                      fontWeight: 700, color: "var(--ink)", lineHeight: 1.6 }}>
                      直近 {formatCount(sales.limit)} 件のみ表示しています。これは全件ではありません。
                    </div>
                  )}

                  <div style={{ marginTop: 20 }}>
                    <div className="h-section">取引一覧</div>
                  </div>
                  <div style={{ marginTop: 10, border: "1.5px solid var(--ink)" }}>
                    {(sales.sales || []).map((s, i) => (
                      <SalesRow key={s.intent_id} sale={s} last={i === sales.sales.length - 1}
                        merchantId={selected} onReported={refreshSales} />
                    ))}
                    {(!sales.sales || sales.sales.length === 0) && (
                      <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
                        fontSize: 13, color: "var(--ink-soft)" }}>まだ売上がありません</div>
                    )}
                  </div>
                </>
              )}
            </>
          )}

          {view === "sales" && !current && (
            <div style={{ padding: "30px 0", textAlign: "center", fontFamily: "var(--font-jp)",
              fontSize: 13, color: "var(--ink-soft)" }}>加盟店が見つかりません</div>
          )}

          {view === "bills" && current && <MoyBillsPage key={current.merchant_id} merchant={current} />}

          {view === "bills" && !current && (
            <div style={{ padding: "30px 0", textAlign: "center", fontFamily: "var(--font-jp)",
              fontSize: 13, color: "var(--ink-soft)" }}>加盟店が見つかりません</div>
          )}
        </div>
      </div>

      {ask && (
        <MoyPinStep zIndex={150} title={ask.title} sub={ask.sub}
          notice="この操作には口座の PIN が必要です。API キーだけでは実行できません。"
          busy={busy} error={askErr ? errLabelFor(askErr) : null} cta={ask.cta}
          onCancel={() => { setAsk(null); setAskErr(null); }} onSubmit={runAsk} />
      )}
    </>
  );
}

Object.assign(window, { MerchantPortal, MoyApiKeyReveal, moyDateLabel, EscrowStageBadge, SalesRow,
  FulfillPanel, SaleEscrowNotes, MoyStaticQrSection, MoyBillsPage, MoyBillRow, MoyNewBillForm,
  MoyBillQr, MoyHeldAlert, moyBillErrText });
