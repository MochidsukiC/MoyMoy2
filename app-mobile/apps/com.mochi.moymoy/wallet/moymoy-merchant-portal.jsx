/* global React, CrystalIcon, EmeGem, formatEme, MoyMoy, MoyPinStep, errLabelFor, msState, msEffect, msRef, fieldStyle, ctaStyle */
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

function MerchantPortal({ open, onClose }) {
  const [list, setList] = msState([]);
  const [loading, setLoading] = msState(true);
  const [loadErr, setLoadErr] = msState(null);
  const [view, setView] = msState("list");   // list | register | detail
  const [selected, setSelected] = msState(null); // merchant_id
  const [revealed, setRevealed] = msState(null); // 一度だけ見せる平文キー
  const [ask, setAsk] = msState(null);       // {title, sub, cta, run(pin)}
  const [askErr, setAskErr] = msState(null);
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState(null);
  const [form, setForm] = msState({ name: "", sub: "" });
  const [limits, setLimits] = msState({ max_open_intents: "", daily_issue_cap: "" });
  const aliveRef = msRef(true);

  async function reload() {
    try {
      const r = await MoyMoy.merchantList();
      if (!aliveRef.current) return;
      if (r.ok) { setList(r.merchants || []); setLoadErr(null); }
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
    reload();
    return () => { aliveRef.current = false; };
  }, [open]);

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

  const nameOk = form.name.trim().length >= 1 && form.name.trim().length <= 32;

  return (
    <>
      <div className="fade-in" style={{ position: "absolute", inset: 0, zIndex: 145,
        background: "var(--bg-white)", display: "flex", flexDirection: "column" }}>
        <div style={{ flexShrink: 0, padding: "56px 18px 14px", background: "var(--moy)", color: "#fff",
          borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
          <div className="paper-noise" style={{ opacity: 0.1 }} />
          <div style={{ position: "relative", zIndex: 1 }}>
            <button
              onClick={() => {
                if (view === "list") { onClose(); return; }
                setView("list"); setErr(null); setRevealed(null);
              }}
              style={{ background: "transparent", border: "none", cursor: "pointer", color: "#fff",
              fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700, letterSpacing: "0.16em",
              display: "flex", alignItems: "center", gap: 6, marginBottom: 8 }}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>
              {view === "list" ? "閉じる" : "一覧へ"}
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

        <div style={{ flex: 1, overflow: "auto", padding: "16px 18px 30px" }}>
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
                      <button key={m.merchant_id}
                        onClick={() => {
                          setSelected(m.merchant_id); setRevealed(null); setErr(null);
                          // 上限の入力欄は店ごと。持ち越すと別の店の値を送りかねない。
                          setLimits({ max_open_intents: "", daily_issue_cap: "" });
                          setView("detail");
                        }}
                        style={{ width: "100%", textAlign: "left", cursor: "pointer", display: "flex",
                        alignItems: "center", gap: 12, padding: "12px 14px", background: "var(--bg-white)",
                        border: "none", borderBottom: i === list.length - 1 ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
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
              <div style={{ marginTop: 16 }}>
                <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>店名（変更不可）</div>
                <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })}
                  maxLength={32} aria-label="店名（登録後は変更できません）"
                  placeholder="ピグルショップ" style={fieldStyle} />
                <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", marginTop: 6 }}>
                  32文字まで。運営を騙る語や、複数の文字体系を混ぜた紛らわしい名前は登録できません。
                </div>
              </div>
              <div style={{ marginTop: 16 }}>
                <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>ひとこと説明（任意）</div>
                <input value={form.sub} onChange={(e) => setForm({ ...form, sub: e.target.value })}
                  maxLength={48} aria-label="お店のひとこと説明（任意）"
                  placeholder="建材と装飾ブロックのお店" style={fieldStyle} />
              </div>
              <button disabled={!nameOk} onClick={doRegister} style={{ ...ctaStyle(!nameOk), marginTop: 20 }}>
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

Object.assign(window, { MerchantPortal, MoyApiKeyReveal, moyDateLabel });
