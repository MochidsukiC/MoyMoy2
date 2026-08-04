/* global React, CrystalIcon, EmeGem, Eme, formatEme, MoyMoyCard, MoyHeader, MoyBottomNav, MoyHome, CompleteOverlay, MoyMoy, TxnRow, PinDots, PinKeypad, AccountMenu, SettingsSheet, fieldStyle, ctaStyle */
/* =====================================================================
   MoyMoy — 画面: 支払う / 送る / チャージ / 履歴 / EC決済の承認 / 加盟店管理
            + キーパッド / 金額入力 / 確認シート / オーケストレータ
   Adapted from Claude Design "MochiOS Mobile.html": the presentational layer
   is unchanged; MoySend takes its list as a prop, and MoyMoyApp is wired
   to the moymoy.cs.mnn backend (MoyMoy SDK) instead of local mock data.

   PIN を集める画面は MoyPinStep 1 つだけで、決済承認・送金/出金の step-up・
   加盟店ポータルがすべてそれを使う。分裂させると、片方だけが個人化シールと
   「決済画面以外で PIN を求めません」を失い、そちらが偽画面と区別のつかない
   画面になる。
   ===================================================================== */

const { useState: msState, useEffect: msEffect, useRef: msRef } = React;

/* ─── client-side enrichment (backend rows carry no design palette/glyph) ── */
const MOY_PALS = ["orange", "green", "blue", "turquoise", "pink", "purple", "emerald", "red"];
function enrichFriend(f, i) {
  return {
    ...f,
    pal: f.pal || MOY_PALS[i % MOY_PALS.length],
    glyph: f.glyph || (f.name ? Array.from(f.name)[0] : "?"),
  };
}

/* ─── 取引時刻ラベル (今日 HH:MM / 昨日 HH:MM / M月D日) ─────────────── */
function moyTime(ts) {
  if (!ts) return "";
  const d = new Date(ts);
  const now = new Date();
  const hm = d.toLocaleTimeString("ja-JP", { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === now.toDateString()) return "今日 " + hm;
  const y = new Date(now);
  y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return "昨日 " + hm;
  return d.getMonth() + 1 + "月" + d.getDate() + "日";
}
function mapTxns(arr) {
  return (arr || []).map((t) => ({ ...t, time: moyTime(t.ts) }));
}

/* ─── Minecraft 風アイテムスロット ─────────────────────────────── */
function MineSlot({ kind, count, size = 56 }) {
  return (
    <div style={{ width: size, height: size, position: "relative", flexShrink: 0,
      background: "#8b8b8b",
      boxShadow: "inset 2px 2px 0 #373737, inset -2px -2px 0 #ffffff",
      display: "grid", placeItems: "center" }}>
      {kind === "block"
        ? <EmeBlockMini size={size * 0.62} />
        : <EmeGem size={size * 0.6} />}
      <span style={{ position: "absolute", right: 3, bottom: 0, fontFamily: "var(--font-mono)",
        fontWeight: 700, fontSize: size * 0.26, color: "#fff", lineHeight: 1,
        textShadow: "2px 2px 0 #3f3f3f" }}>{count}</span>
    </div>
  );
}
/* 小さなエメラルドブロック (ピクセル質感) */
function EmeBlockMini({ size = 34 }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" style={{ display: "block", imageRendering: "pixelated" }}>
      <rect width="32" height="32" fill="#1f9e57" />
      <rect x="1" y="1" width="30" height="30" fill="#27b566" />
      <polygon points="16,3 29,16 16,29 3,16" fill="#2ECC71" />
      <polygon points="16,3 29,16 16,16 3,16" fill="#3fd981" />
      <polygon points="16,7 25,16 16,25 7,16" fill="#5ceb95" />
      <polygon points="16,7 25,16 16,16" fill="#7df2ab" />
      <rect x="3" y="3" width="4" height="4" fill="#bdf7d6" opacity="0.8" />
    </svg>
  );
}

/* ─── テンキー ───────────────────────────────────────────────────── */
function MoyKeypad({ onPress }) {
  const keys = ["1","2","3","4","5","6","7","8","9","00","0","⌫"];
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 1,
      background: "var(--ink)", border: "1.5px solid var(--ink)" }}>
      {keys.map(k => (
        <button key={k} onClick={() => onPress(k)} style={{
          height: 58, background: "var(--bg-white)", border: "none", cursor: "pointer",
          fontFamily: "var(--font-mono)", fontWeight: 700, fontSize: k === "⌫" ? 20 : 24,
          color: "var(--ink)", letterSpacing: "0.02em" }}>{k}</button>
      ))}
    </div>
  );
}

/* ─── 個人化シール ─────────────────────────────────────────────────
   ユーザーが選んだ絵文字と短い言葉を、PIN を入力する画面に必ず表示する。
   この組み合わせはこの端末のこの口座にしか保存されていないので、MoyMoy を
   模した偽の PIN 画面は再現できない。「いつものシールが出ていない」ことを
   ユーザー自身がフィッシングの検出に使えるようにするための仕掛けであり、
   装飾ではない。口座ごとに持つのは、口座を切り替えたときに前の口座のシールが
   残っていると「合っているはず」の判断材料が濁るため。 */
const SEAL_GLYPHS = ["🍡", "🐈", "🍀", "⭐", "🍎", "🌙", "🐧", "🔥", "🎈", "🍄", "🐝", "🎵"];
const sealKey = (accountId) => "moymoy.seal.v1." + accountId;

async function loadSeal(accountId) {
  if (!accountId) return null;
  const raw = await MoyMoy.store.get(sealKey(accountId));
  if (!raw) return null;
  try {
    const o = JSON.parse(raw);
    return o && o.glyph ? { glyph: o.glyph, phrase: o.phrase || "" } : null;
  } catch (e) {
    console.error("MoyMoy: stored seal is corrupt JSON; treating it as unset", e);
    return null;
  }
}
function saveSeal(accountId, seal) {
  return MoyMoy.store.set(sealKey(accountId), JSON.stringify(seal));
}

/* シールの表示。未設定でも「未設定」と明示する — 何も描かないと、シールを
   出せない偽画面と見分けがつかなくなる。 */
function MoySealBadge({ seal, onSetup }) {
  if (!seal) {
    return (
      <div style={{ border: "1.5px dashed var(--ink-soft)", background: "rgba(0,0,0,0.03)",
        padding: "10px 12px", display: "flex", alignItems: "center", gap: 10 }}>
        <div style={{ flex: 1, minWidth: 0, fontFamily: "var(--font-jp)", fontSize: 11,
          color: "var(--ink-soft)", lineHeight: 1.6 }}>
          あなた専用のシールが未設定です。設定すると、この画面が本物かどうかを
          ひと目で見分けられます。
          {!onSetup && "「設定 › あなたのシール」から登録できます。"}
        </div>
        {onSetup && (
          <button onClick={onSetup} style={{ flexShrink: 0, border: "1.5px solid var(--moy-deep)",
            background: "var(--moy-mint)", color: "var(--moy-deep)", cursor: "pointer",
            padding: "7px 10px", fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 }}>
            設定する
          </button>
        )}
      </div>
    );
  }
  return (
    <div style={{ border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
      padding: "8px 12px", display: "flex", alignItems: "center", gap: 10 }}>
      <span style={{ fontSize: 24, lineHeight: 1 }}>{seal.glyph}</span>
      <div style={{ minWidth: 0 }}>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, letterSpacing: "0.14em",
          color: "var(--moy-deep)" }}>YOUR SEAL</div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700,
          color: "var(--ink)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {seal.phrase || "（言葉なし）"}
        </div>
      </div>
    </div>
  );
}

/* シールの設定シート。絵文字を選び、短い言葉を添える。 */
function MoySealSetup({ open, seal, onSave, onClose }) {
  const [glyph, setGlyph] = msState((seal && seal.glyph) || SEAL_GLYPHS[0]);
  const [phrase, setPhrase] = msState((seal && seal.phrase) || "");
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState("");
  if (!open) return null;
  async function save() {
    setBusy(true); setErr("");
    const ok = await onSave({ glyph, phrase: phrase.trim().slice(0, 20) });
    if (!ok) { setErr("シールを保存できませんでした。もう一度お試しください"); setBusy(false); return; }
    setBusy(false);
    onClose();
  }
  return (
    <div style={{ position: "absolute", inset: 0, zIndex: 170, display: "flex", flexDirection: "column",
      justifyContent: "flex-end", background: "rgba(10,30,18,0.45)" }} onClick={busy ? undefined : onClose}>
      <div className="slide-up" onClick={(e) => e.stopPropagation()} style={{ background: "var(--bg-white)",
        borderTop: "1.5px solid var(--ink)", padding: "18px 18px 26px" }}>
        <div style={{ width: 44, height: 4, background: "var(--ink)", margin: "0 auto 14px", opacity: 0.4 }} />
        <div className="eyebrow" style={{ color: "var(--moy-deep)", textAlign: "center" }}>あなたのシール</div>
        <div style={{ marginTop: 10, fontFamily: "var(--font-jp)", fontSize: 12,
          color: "var(--ink-soft)", lineHeight: 1.7 }}>
          PIN を入力する画面に、いつもこの絵と言葉が出ます。出ていなければ、それは
          MoyMoy の画面ではありません。
        </div>
        <div style={{ marginTop: 14, display: "grid", gridTemplateColumns: "repeat(6, 1fr)", gap: 6 }}>
          {SEAL_GLYPHS.map((g) => (
            <button key={g} onClick={() => setGlyph(g)} style={{ height: 44, cursor: "pointer",
              border: g === glyph ? "1.5px solid var(--moy-deep)" : "1.5px solid rgba(0,0,0,0.15)",
              background: g === glyph ? "var(--moy-mint)" : "var(--bg-white)", fontSize: 22, padding: 0 }}>
              {g}
            </button>
          ))}
        </div>
        <div style={{ marginTop: 14 }}>
          <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>短い言葉</div>
          <input value={phrase} onChange={(e) => setPhrase(e.target.value)} maxLength={20}
            aria-label="シールの短い言葉"
            placeholder="あおいそら" style={{ width: "100%", padding: "12px 14px",
            border: "1.5px solid var(--ink)", background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)",
            fontFamily: "var(--font-jp)", fontSize: 15, color: "var(--ink)", outline: "none",
            boxSizing: "border-box" }} />
        </div>
        <div style={{ marginTop: 14 }}>
          <MoySealBadge seal={{ glyph, phrase }} />
        </div>
        {err && (
          <div style={{ marginTop: 10, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
            color: "var(--carle-red)" }}>{err}</div>
        )}
        <button disabled={busy} onClick={save} style={{ width: "100%", marginTop: 16, padding: "15px",
          border: "1.5px solid #000", background: busy ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: busy ? "none" : "4px 4px 0 #0B5A33", cursor: busy ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 16 }}>
          {busy ? "保存中…" : "このシールにする"}
        </button>
        <button disabled={busy} onClick={onClose} style={{ width: "100%", marginTop: 8, padding: "12px",
          border: "none", background: "transparent", cursor: busy ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>
          あとで
        </button>
      </div>
    </div>
  );
}

/* ─── PIN 入力ステップ (決済承認 / 送金の step-up / 加盟店ポータル 共用) ──
   PIN を集める画面はここ 1 箇所しかない。分裂させると、片方だけがシールや
   注意文を失い、そちらが偽画面と見分けのつかない画面になる。
   注意文とシールを props にしてあるのは、加盟店ポータルの PIN 確認で
   「決済画面以外で PIN を求めません」と表示すると、その文自身と矛盾するため。

   シールは表示するだけで、ここから設定させる導線は**出さない**。理由は2つ:
   ① 入力途中の PIN を抱えたまま別画面を開かせない（PIN が state に残る時間は
   短いほどよい）。② シールは「攻撃者が知り得ない、事前に信頼できる文脈で
   決めた合言葉」であることに価値がある。PIN 画面から登録させると、偽画面が
   「未設定ですね、設定しましょう」とユーザーにシールを決めさせ、それを見せ返す
   経路ができてしまい、前提そのものが崩れる。登録は設定画面だけで行う。 */
function MoyPinStep({ title, sub, summary, notice, showSeal, seal, busy, error,
  cta, onCancel, onSubmit, zIndex = 150 }) {
  const [pin, setPin] = msState("");
  const press = (k) => setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  const canSubmit = pin.length >= 4 && !busy;
  return (
    <div className="fade-in" style={{ position: "absolute", inset: 0, zIndex,
      background: "var(--bg-white)", display: "flex", flexDirection: "column" }}>
      <div style={{ flexShrink: 0, padding: "56px 18px 14px", background: "var(--moy)", color: "#fff",
        borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
        <div className="paper-noise" style={{ opacity: 0.1 }} />
        <div style={{ position: "relative", zIndex: 1 }}>
          <button onClick={busy ? undefined : onCancel} style={{ background: "transparent", border: "none",
            cursor: busy ? "default" : "pointer", color: "#fff", fontFamily: "var(--font-mono)",
            fontSize: 11, fontWeight: 700, letterSpacing: "0.16em", display: "flex", alignItems: "center",
            gap: 6, marginBottom: 8 }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>戻る
          </button>
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <EmeGem size={24} />
            <div>
              <div style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 20,
                letterSpacing: "-0.02em" }}>{title || "PIN を入力"}</div>
              {sub && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, opacity: 0.9 }}>{sub}</div>}
            </div>
          </div>
        </div>
      </div>

      <div style={{ flex: 1, overflow: "auto", padding: "16px 18px 24px" }}>
        {summary}
        {showSeal && (
          <div style={{ marginTop: summary ? 14 : 0 }}>
            <MoySealBadge seal={seal} />
          </div>
        )}
        {notice && (
          <div style={{ marginTop: 12, padding: "9px 12px", border: "1.5px solid var(--ink)",
            background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-jp)", fontSize: 12,
            fontWeight: 700, color: "var(--ink)", lineHeight: 1.6 }}>{notice}</div>
        )}

        <div style={{ marginTop: 20, display: "flex", flexDirection: "column", alignItems: "center", gap: 16 }}>
          <PinDots len={pin.length} />
          {error && (
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
              color: "var(--carle-red)", textAlign: "center", lineHeight: 1.6 }}>{error}</div>
          )}
          <div style={{ width: "100%", maxWidth: 320 }}>
            <PinKeypad onPress={press} />
            <button disabled={!canSubmit} onClick={() => { const p = pin; setPin(""); onSubmit(p); }}
              style={{ width: "100%", marginTop: 14, padding: "16px", border: "1.5px solid #000",
              background: canSubmit ? "var(--moy)" : "#bdbdbd", color: "#fff",
              boxShadow: canSubmit ? "4px 4px 0 #0B5A33" : "none", cursor: canSubmit ? "pointer" : "default",
              fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17, letterSpacing: "0.04em",
              display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
              <EmeGem size={20} /> {busy ? "処理中…" : cta || "確認する"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/* ─── 相手カード (リスト行) ──────────────────────────────────────── */
function TargetRow({ item, onClick, trailing }) {
  return (
    <button onClick={onClick} style={{ width: "100%", textAlign: "left", cursor: "pointer",
      display: "flex", alignItems: "center", gap: 12, padding: "12px 16px",
      borderBottom: "1px solid rgba(0,0,0,0.08)", background: "var(--bg-white)", border: "none",
      borderBottomWidth: 1, borderBottomStyle: "solid", borderBottomColor: "rgba(0,0,0,0.08)" }}>
      <div style={{ width: 42, height: 42, flexShrink: 0 }}>
        <CrystalIcon palette={item.pal} glyph={item.glyph} size={42} />
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>{item.name}</div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)",
          letterSpacing: "0.05em", marginTop: 1 }}>{item.sub}</div>
      </div>
      {trailing}
    </button>
  );
}

/* ─── 支払う ─────────────────────────────────────────────────────
   支払いはこの画面からは始まらない。金額と宛先を決めるのはお店であり、
   ウォレットはお店が発行した請求を承認するだけ — クライアントが金額と
   受取先を指定できる `/wallet/pay` が廃止されたのはそのためで、この画面に
   金額入力を残すと、その廃止した経路をアプリ側に作り直すことになる。
   なので、ここは「どう支払うか」の説明と、最近の支払いの控えに徹する。 */
function MoyPay({ txns = [] }) {
  const recent = txns.filter((t) => t.kind === "pay").slice(0, 6);
  const steps = [
    ["1", "お店のアプリで買う", "商品を選んで「MoyMoy で支払う」を選びます。"],
    ["2", "MoyMoy が開く", "お店・金額・内容が表示されます。表示内容は MoyMoy が記録したもので、お店のアプリからは書き換えられません。"],
    ["3", "PIN で承認", "内容を確かめて PIN を入力すると支払いが完了し、お店に戻ります。"],
  ];
  return (
    <div style={{ padding: "18px 16px 120px" }}>
      <div style={{ textAlign: "center" }}>
        <EmeGem size={44} style={{ margin: "0 auto 10px" }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 16, fontWeight: 800, color: "var(--ink)" }}>
          お支払いはお店のアプリから
        </div>
        <div style={{ marginTop: 6, fontFamily: "var(--font-jp)", fontSize: 12,
          color: "var(--ink-soft)", lineHeight: 1.8 }}>
          MoyMoy 側から金額を入力して支払うことはできません。
        </div>
      </div>

      <div style={{ marginTop: 18, border: "1.5px solid var(--ink)" }}>
        {steps.map(([n, title, body], i) => (
          <div key={n} style={{ display: "flex", gap: 12, padding: "12px 14px", background: "var(--bg-white)",
            borderBottom: i === steps.length - 1 ? "none" : "1px solid rgba(0,0,0,0.08)" }}>
            <span style={{ flexShrink: 0, width: 26, height: 26, display: "grid", placeItems: "center",
              background: "var(--moy-mint)", border: "1.5px solid var(--moy-deep)", color: "var(--moy-deep)",
              fontFamily: "var(--font-mono)", fontSize: 13, fontWeight: 700 }}>{n}</span>
            <div style={{ minWidth: 0 }}>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700, color: "var(--ink)" }}>{title}</div>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)",
                lineHeight: 1.7, marginTop: 2 }}>{body}</div>
            </div>
          </div>
        ))}
      </div>

      {/* 承認画面と同じ文言をここにも置く。ユーザーが「MoyMoy はこう振る舞う」と
          知っている状態を作るのが、偽の PIN 画面に対する最後の防御になる。 */}
      <div style={{ marginTop: 14, padding: "11px 13px", border: "1.5px solid var(--ink)",
        background: "rgba(0,0,0,0.04)", fontFamily: "var(--font-jp)", fontSize: 12,
        fontWeight: 700, color: "var(--ink)", lineHeight: 1.7 }}>
        MoyMoy は決済画面以外で PIN を求めません。ほかのアプリや画面で PIN を
        聞かれたら、入力せずに閉じてください。
      </div>

      <div style={{ marginTop: 22 }}>
        <div className="h-section">最近の支払い</div>
      </div>
      <div style={{ marginTop: 10, border: "1.5px solid var(--ink)" }}>
        {recent.map((t, i) => <TxnRow key={t.id} t={t} last={i === recent.length - 1} />)}
        {recent.length === 0 && (
          <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
            fontSize: 13, color: "var(--ink-soft)" }}>まだ支払いはありません</div>
        )}
      </div>
    </div>
  );
}

/* ─── 送る (MoyMoy ID 指定 + 最近の相手) ────────────────────────── */
function MoySend({ friends = [], onPick }) {
  const [qy, setQy] = msState("");
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState("");
  const cleanHandle = qy.trim().replace(/^@/, "");
  const handleValid = /^[A-Za-z0-9_]{3,20}$/.test(cleanHandle);
  const shown = qy
    ? friends.filter(f => (f.name + " " + (f.sub || "") + " " + (f.handle || "")).toLowerCase().includes(cleanHandle.toLowerCase()))
    : friends;

  async function resolveAndPick() {
    const h = cleanHandle;
    if (!handleValid) { setErr("MoyMoy ID（半角英数字と _ の3〜20文字）を入力してください"); return; }
    setBusy(true); setErr("");
    try {
      const r = await MoyMoy.lookup(h);
      if (r.ok && r.account) {
        const name = r.account.display_name || ("@" + r.account.handle);
        onPick({ id: r.account.account_id, handle: r.account.handle, name,
          sub: "@" + r.account.handle, pal: "emerald", glyph: Array.from(name)[0] || "?" });
      } else {
        setErr("@" + h + " が見つかりません");
      }
    } catch (e) {
      console.warn("MoyMoy: handle lookup failed (network/server/parse)", e);
      setErr("検索に失敗しました");
    }
    setBusy(false);
  }

  return (
    <div style={{ padding: "0 0 120px" }}>
      <div style={{ padding: "16px 16px 10px" }}>
        <div style={{ display: "flex", gap: 8 }}>
          <div style={{ flex: 1, display: "flex", alignItems: "center", gap: 6,
            border: "1.5px solid var(--ink)", background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)",
            padding: "0 10px" }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 16, fontWeight: 700, color: "var(--ink-soft)" }}>@</span>
            <input value={qy} onChange={e => { setQy(e.target.value); setErr(""); }}
              onKeyDown={e => { if (e.key === "Enter") resolveAndPick(); }}
              placeholder="MoyMoy ID で送る"
              style={{ flex: 1, padding: "12px 4px", border: "none", background: "transparent",
              fontFamily: "var(--font-jp)", fontSize: 14, color: "var(--ink)", outline: "none" }} />
          </div>
          <button disabled={busy || !handleValid} onClick={resolveAndPick} style={{
            border: "1.5px solid #000", background: busy || !handleValid ? "#bdbdbd" : "var(--moy)",
            color: "#fff", boxShadow: busy || !handleValid ? "none" : "3px 3px 0 #0B5A33",
            cursor: busy || !handleValid ? "default" : "pointer", padding: "0 16px",
            fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 15 }}>送る</button>
        </div>
        {err && <div style={{ marginTop: 8, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
      </div>
      <div style={{ padding: "4px 16px 8px" }}>
        <div className="h-section">最近の相手</div>
      </div>
      <div style={{ borderTop: "1.5px solid var(--ink)", borderBottom: "1.5px solid var(--ink)" }}>
        {shown.map(f => (
          <TargetRow key={f.id} item={f} onClick={() => onPick(f)}
            trailing={<span style={{ fontFamily: "var(--font-mono)", fontSize: 18, color: "var(--moy-deep)" }}>›</span>} />
        ))}
        {shown.length === 0 && (
          <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
            fontSize: 13, color: "var(--ink-soft)" }}>
            {friends.length === 0 ? "まだ取引相手がいません。上の欄に MoyMoy ID を入力して送れます。" : "該当する相手がいません"}
          </div>
        )}
      </div>
    </div>
  );
}

/* ─── 金額入力 (支払う / 送る 共用) ─────────────────────────────── */
function MoyAmountEntry({ kind, target, balance, onCancel, onNext }) {
  const [amt, setAmt] = msState(0);
  const press = (k) => {
    setAmt(v => {
      if (k === "⌫") return Math.floor(v / 10);
      const add = k === "00" ? "00" : k;
      const next = Number(String(v) + add);
      return next > 9999999 ? v : next;
    });
  };
  const over = amt > balance;
  const verb = kind === "send" ? "送る" : "支払う";
  return (
    <div className="fade-in" style={{ position: "absolute", inset: 0, zIndex: 90, background: "var(--bg-white)",
      display: "flex", flexDirection: "column" }}>
      {/* header */}
      <div style={{ flexShrink: 0, padding: "56px 18px 14px", background: "var(--moy)",
        color: "#fff", borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
        <div className="paper-noise" style={{ opacity: 0.1 }} />
        <div style={{ position: "relative", zIndex: 1, display: "flex", alignItems: "center", gap: 12 }}>
          <button onClick={onCancel} style={{ background: "transparent", border: "none", cursor: "pointer",
            color: "#fff", fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700, letterSpacing: "0.16em",
            display: "flex", alignItems: "center", gap: 6 }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>戻る
          </button>
          <div style={{ marginLeft: "auto", display: "flex", alignItems: "center", gap: 10 }}>
            <div style={{ textAlign: "right" }}>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700 }}>{target.name}</div>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, opacity: 0.85, letterSpacing: "0.08em" }}>{verb}先</div>
            </div>
            <div style={{ width: 38, height: 38 }}><CrystalIcon palette={target.pal} glyph={target.glyph} size={38} /></div>
          </div>
        </div>
      </div>

      {/* amount display */}
      <div style={{ flex: 1, display: "flex", flexDirection: "column", alignItems: "center",
        justifyContent: "center", padding: "10px 22px" }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>{verb}金額</div>
        <Eme amount={amt} size={58} color={over ? "var(--carle-red)" : "var(--ink)"} />
        <div style={{ marginTop: 10, fontFamily: "var(--font-mono)", fontSize: 11,
          color: over ? "var(--carle-red)" : "var(--ink-soft)", letterSpacing: "0.04em" }}>
          {over ? "残高が不足しています" : `利用可能残高  ${formatEme(balance)} エメ`}
        </div>
        {/* quick chips */}
        <div style={{ display: "flex", gap: 8, marginTop: 18 }}>
          {[100, 500, 1000].map(q => (
            <button key={q} onClick={() => setAmt(v => Math.min(9999999, v + q))} style={{
              border: "1.5px solid var(--ink)", background: "var(--bg-white)", boxShadow: "2px 2px 0 var(--ink)",
              padding: "7px 12px", cursor: "pointer", fontFamily: "var(--font-mono)", fontSize: 12, fontWeight: 700 }}>
              +{formatEme(q)}
            </button>
          ))}
          <button onClick={() => setAmt(balance)} style={{ border: "1.5px solid var(--moy-deep)",
            background: "var(--moy-mint)", padding: "7px 12px", cursor: "pointer", color: "var(--moy-deep)",
            fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 }}>全額</button>
        </div>
      </div>

      {/* keypad + cta */}
      <div style={{ padding: "0 16px 24px" }}>
        <MoyKeypad onPress={press} />
        <button disabled={amt <= 0 || over} onClick={() => onNext(amt)} style={{
          width: "100%", marginTop: 12, padding: "16px", border: "1.5px solid #000",
          background: amt <= 0 || over ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: amt <= 0 || over ? "none" : "4px 4px 0 #0B5A33", cursor: amt <= 0 || over ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17, letterSpacing: "0.04em",
          display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
          <EmeGem size={20} /> {verb}内容を確認
        </button>
      </div>
    </div>
  );
}

/* ─── エメ入出金セグメント (チャージ / 出金) ───────────────────── */
function MoyEmeModeSwitch({ mode, onMode }) {
  const segs = [["charge", "チャージ"], ["withdraw", "出金"]];
  return (
    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", border: "1.5px solid var(--ink)",
      margin: "12px 16px 0" }}>
      {segs.map(([id, label], i) => {
        const active = mode === id;
        return (
          <button key={id} onClick={() => onMode(id)} style={{
            padding: "10px 0", border: "none", borderLeft: i === 0 ? "none" : "1.5px solid var(--ink)",
            background: active ? "var(--moy-deep)" : "var(--bg-white)", color: active ? "#fff" : "var(--ink)",
            cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 14 }}>
            {label}
          </button>
        );
      })}
    </div>
  );
}

/* ─── チャージ画面 (インベントリの手持ちエメラルドのみ) ──────────── */
function MoyCharge({ balance, inv, canCharge, invStatus, attesting, onConfirmCharacter, onConfirm }) {
  const available = inv.emeralds + inv.blocks * 9; // 9エメ = 1ブロック
  const [amt, setAmt] = msState(0);
  const press = (k) => {
    setAmt(v => {
      if (k === "⌫") return Math.floor(v / 10);
      const add = k === "00" ? "00" : k;
      const next = Number(String(v) + add);
      return next > 9999999 ? v : next;
    });
  };
  const over = amt > available;

  if (!canCharge) {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          チャージは現在利用できません
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          エメラルドのチャージには Minecraft サーバーへの接続が必要です。<br />
          サーバーに参加してから、もう一度お試しください。
        </div>
      </div>
    );
  }

  // The character has to be confirmed before the backend knows whose inventory
  // to read. The confirmation is a user action (it raises the OS consent modal),
  // so it is a BUTTON here rather than something the screen does on open.
  if (invStatus === "attestation_required" || (invStatus && invStatus.indexOf("attest_") === 0)) {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          キャラクターの確認が必要です
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          どのキャラクターのエメラルドを使うかを、<br />
          MochiOS に確認してもらいます。
        </div>
        {invStatus !== "attestation_required" && (
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)", marginTop: 12, lineHeight: 1.7 }}>
            {errLabel(invStatus)}
          </div>
        )}
        <button disabled={attesting} onClick={onConfirmCharacter} style={{ marginTop: 20, padding: "14px 22px",
          border: "1.5px solid #000", background: attesting ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: attesting ? "none" : "4px 4px 0 #0B5A33", cursor: attesting ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 15 }}>
          {attesting ? "確認中…" : "キャラクターを確認"}
        </button>
      </div>
    );
  }

  // The backend distinguishes "couldn't reach the character" from a genuine 0, so
  // don't show a misleading empty inventory — say why and let the user retry.
  if (invStatus === "character_unreachable") {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          手持ちのエメラルドを取得できません
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          Minecraft のキャラクターに接続できませんでした。<br />
          ゲームにログインしているか、サーバー側の連携（mod）設定をご確認ください。
        </div>
      </div>
    );
  }

  return (
    <div style={{ padding: "16px 16px 120px" }}>
      <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>チャージ元 · インベントリ</div>

      {/* inventory panel — Minecraft hotbar style */}
      <div style={{ marginTop: 8, border: "1.5px solid var(--ink)", boxShadow: "3px 3px 0 var(--ink)",
        background: "var(--bg-white)", padding: "14px" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 14 }}>
          <div style={{ display: "flex", gap: 8 }}>
            <MineSlot kind="emerald" count={inv.emeralds} />
            <MineSlot kind="block" count={inv.blocks} />
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 600, color: "var(--ink-soft)" }}>
              手持ち {inv.emeralds} エメラルド ＋ {inv.blocks} ブロック
            </div>
            <div style={{ marginTop: 3 }}>
              <Eme amount={available} size={22} color="var(--moy-deep)" />
            </div>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, color: "var(--ink-soft)",
              letterSpacing: "0.08em", marginTop: 2 }}>CHARGEABLE · 換算可能額</div>
          </div>
        </div>
      </div>

      <div style={{ marginTop: 18, textAlign: "center" }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>チャージ金額</div>
        <Eme amount={amt} size={48} color={over ? "var(--carle-red)" : "var(--ink)"} />
        <div style={{ marginTop: 6, fontFamily: "var(--font-mono)", fontSize: 11,
          color: over ? "var(--carle-red)" : "var(--ink-soft)" }}>
          {over ? "手持ちのエメラルドが不足しています" : `チャージ後  ${formatEme(balance + amt)} エメ`}
        </div>
      </div>
      <div style={{ display: "flex", gap: 8, marginTop: 14, justifyContent: "center", flexWrap: "wrap" }}>
        <button onClick={() => setAmt(v => Math.min(available, v + 9))} style={chipStyle}>＋1ブロック</button>
        <button onClick={() => setAmt(v => Math.min(available, v + 64))} style={chipStyle}>＋1スタック</button>
        <button onClick={() => setAmt(available)} style={{ border: "1.5px solid var(--moy-deep)",
          background: "var(--moy-mint)", padding: "7px 12px", cursor: "pointer", color: "var(--moy-deep)",
          fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 }}>全部</button>
      </div>

      <div style={{ marginTop: 16 }}>
        <MoyKeypad onPress={press} />
        <button disabled={amt <= 0 || over} onClick={() => onConfirm(amt)} style={{
          width: "100%", marginTop: 12, padding: "16px", border: "1.5px solid #000",
          background: amt <= 0 || over ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: amt <= 0 || over ? "none" : "4px 4px 0 #0B5A33", cursor: amt <= 0 || over ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17, letterSpacing: "0.04em",
          display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
          <EmeGem size={20} /> エメラルドをチャージ
        </button>
      </div>
    </div>
  );
}
const chipStyle = { border: "1.5px solid var(--ink)", background: "var(--bg-white)", boxShadow: "2px 2px 0 var(--ink)",
  padding: "7px 12px", cursor: "pointer", fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 };

/* ─── 出金画面 (残高 → ゲーム内エメラルドで受け取る) ─────────────── */
function MoyWithdraw({ balance, canCharge, invStatus, attesting, onConfirmCharacter, onConfirm }) {
  const cap = Math.min(balance, 20736); // 1回の出金上限 = 20,736エメ（インベントリ1個分）
  const [amt, setAmt] = msState(0);
  const press = (k) => {
    setAmt(v => {
      if (k === "⌫") return Math.floor(v / 10);
      const add = k === "00" ? "00" : k;
      const next = Number(String(v) + add);
      return next > 9999999 ? v : next;
    });
  };
  const overBalance = amt > balance;
  const overLimit = !overBalance && amt > 20736;
  const over = overBalance || overLimit;

  if (!canCharge) {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          出金は現在利用できません
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          出金には Minecraft サーバーへの接続が必要です。<br />
          サーバーに参加してから、もう一度お試しください。
        </div>
      </div>
    );
  }

  // Withdraw doesn't need to READ the character's inventory — there is nothing
  // to read, it only WRITES emeralds into it — but it still needs the character
  // confirmed first: this is how the user is shown, before approving, which
  // character is about to receive the emeralds.
  if (invStatus === "attestation_required" || (invStatus && invStatus.indexOf("attest_") === 0)) {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          キャラクターの確認が必要です
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          どのキャラクターにエメラルドを渡すかを、<br />
          MochiOS に確認してもらいます。
        </div>
        {invStatus !== "attestation_required" && (
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)", marginTop: 12, lineHeight: 1.7 }}>
            {errLabel(invStatus)}
          </div>
        )}
        <button disabled={attesting} onClick={onConfirmCharacter} style={{ marginTop: 20, padding: "14px 22px",
          border: "1.5px solid #000", background: attesting ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: attesting ? "none" : "4px 4px 0 #0B5A33", cursor: attesting ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 15 }}>
          {attesting ? "確認中…" : "キャラクターを確認"}
        </button>
      </div>
    );
  }

  // Same distinction as MoyCharge: the mod couldn't be reached, so don't imply
  // there is simply nothing to withdraw — say why and let the user retry.
  if (invStatus === "character_unreachable") {
    return (
      <div style={{ padding: "40px 24px 120px", textAlign: "center" }}>
        <EmeGem size={56} style={{ margin: "0 auto 16px", opacity: 0.5 }} />
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>
          受け取り先のキャラクターを確認できません
        </div>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", marginTop: 8, lineHeight: 1.7 }}>
          Minecraft のキャラクターに接続できませんでした。<br />
          ゲームにログインしているか、サーバー側の連携（mod）設定をご確認ください。
        </div>
      </div>
    );
  }

  const blocks = Math.floor(amt / 9);
  const rest = amt % 9;

  return (
    <div style={{ padding: "16px 16px 120px" }}>
      <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>受け取り内容 · プレビュー</div>

      {/* preview panel — Minecraft hotbar style */}
      <div style={{ marginTop: 8, border: "1.5px solid var(--ink)", boxShadow: "3px 3px 0 var(--ink)",
        background: "var(--bg-white)", padding: "14px" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 14 }}>
          <div style={{ display: "flex", gap: 8 }}>
            <MineSlot kind="block" count={blocks} />
            <MineSlot kind="emerald" count={rest} />
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 600, color: "var(--ink-soft)" }}>
              {blocks} ブロック ＋ {rest} エメラルドを受け取ります
            </div>
            <div style={{ marginTop: 3 }}>
              <Eme amount={amt} size={22} color="var(--moy-deep)" />
            </div>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, color: "var(--ink-soft)",
              letterSpacing: "0.08em", marginTop: 2 }}>RECEIVE · 受け取り額</div>
          </div>
        </div>
      </div>

      <div style={{ marginTop: 18, textAlign: "center" }}>
        <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>出金金額</div>
        <Eme amount={amt} size={48} color={over ? "var(--carle-red)" : "var(--ink)"} />
        <div style={{ marginTop: 6, fontFamily: "var(--font-mono)", fontSize: 11,
          color: over ? "var(--carle-red)" : "var(--ink-soft)" }}>
          {overBalance ? "残高が不足しています"
            : overLimit ? "1回の出金は 20,736 エメまでです"
            : `出金後残高  ${formatEme(balance - amt)} エメ`}
        </div>
      </div>
      <div style={{ display: "flex", gap: 8, marginTop: 14, justifyContent: "center", flexWrap: "wrap" }}>
        <button onClick={() => setAmt(v => Math.min(cap, v + 9))} style={chipStyle}>＋1ブロック</button>
        <button onClick={() => setAmt(v => Math.min(cap, v + 64))} style={chipStyle}>＋1スタック</button>
        <button onClick={() => setAmt(cap)} style={{ border: "1.5px solid var(--moy-deep)",
          background: "var(--moy-mint)", padding: "7px 12px", cursor: "pointer", color: "var(--moy-deep)",
          fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700 }}>全額</button>
      </div>

      <div style={{ marginTop: 16 }}>
        <MoyKeypad onPress={press} />
        <button disabled={amt <= 0 || over} onClick={() => onConfirm(amt)} style={{
          width: "100%", marginTop: 12, padding: "16px", border: "1.5px solid #000",
          background: amt <= 0 || over ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: amt <= 0 || over ? "none" : "4px 4px 0 #0B5A33", cursor: amt <= 0 || over ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17, letterSpacing: "0.04em",
          display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
          <EmeGem size={20} /> エメラルドを受け取る
        </button>
      </div>
    </div>
  );
}

/* ─── 履歴 ───────────────────────────────────────────────────────── */
function MoyHistory({ txns }) {
  const [filter, setFilter] = msState("all");
  const tabs = [["all", "すべて"], ["pay", "支払い"], ["send", "送金"], ["charge", "チャージ"], ["withdraw", "出金"]];
  const shown = filter === "all" ? txns : txns.filter(t => t.kind === filter);
  return (
    <div style={{ padding: "16px 16px 120px" }}>
      <div style={{ display: "flex", gap: 8, marginBottom: 14 }}>
        {tabs.map(([id, label]) => (
          <button key={id} onClick={() => setFilter(id)} style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
            padding: "6px 12px", border: "1.5px solid var(--ink)", cursor: "pointer",
            background: filter === id ? "var(--moy-deep)" : "var(--bg-white)", color: filter === id ? "#fff" : "var(--ink)" }}>{label}</button>
        ))}
      </div>
      <div style={{ border: "1.5px solid var(--ink)" }}>
        {shown.map((t, i) => <TxnRow key={t.id} t={t} last={i === shown.length - 1} />)}
        {shown.length === 0 && (
          <div style={{ padding: 24, textAlign: "center", fontFamily: "var(--font-jp)",
            fontSize: 13, color: "var(--ink-soft)" }}>取引はありません</div>
        )}
      </div>
    </div>
  );
}

/* ─── 確認シート ─────────────────────────────────────────────────── */
function MoyConfirmSheet({ kind, target, amount, balance, busy, error, onCancel, onConfirm }) {
  const verb = kind === "send" ? "送金" : kind === "charge" ? "チャージ" : kind === "withdraw" ? "出金" : "支払い";
  return (
    <div style={{ position: "absolute", inset: 0, zIndex: 110, display: "flex", flexDirection: "column",
      justifyContent: "flex-end", background: "rgba(10,30,18,0.45)" }} onClick={busy ? undefined : onCancel}>
      <div className="slide-up" onClick={e => e.stopPropagation()} style={{ background: "var(--bg-white)",
        borderTop: "1.5px solid var(--ink)", padding: "20px 18px 30px", position: "relative" }}>
        <div style={{ width: 44, height: 4, background: "var(--ink)", margin: "0 auto 16px", opacity: 0.4 }} />
        <div className="eyebrow" style={{ color: "var(--moy-deep)", textAlign: "center" }}>{verb}内容の確認</div>

        {target && (
          <div style={{ display: "flex", alignItems: "center", gap: 12, marginTop: 16,
            padding: "12px 0", borderBottom: "1px solid rgba(0,0,0,0.1)" }}>
            <div style={{ width: 44, height: 44 }}><CrystalIcon palette={target.pal} glyph={target.glyph} size={44} /></div>
            <div>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700 }}>{target.name}</div>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)" }}>{target.sub}</div>
            </div>
          </div>
        )}

        <div style={{ textAlign: "center", padding: "22px 0 8px" }}>
          <Eme amount={amount} size={52} />
        </div>

        <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)",
          display: "flex", justifyContent: "space-between", padding: "10px 0",
          borderTop: "1px solid rgba(0,0,0,0.1)" }}>
          <span>{kind === "charge" ? "チャージ後残高" : kind === "withdraw" ? "出金後残高" : "支払い後残高"}</span>
          <span style={{ color: "var(--ink)", fontWeight: 700 }}>
            {formatEme(kind === "charge" ? balance + amount : balance - amount)} エメ
          </span>
        </div>

        {error && (
          <div style={{ marginTop: 10, padding: "10px 12px", border: "1.5px solid var(--carle-red)",
            background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
            fontWeight: 700, color: "var(--carle-red)", textAlign: "center" }}>{error}</div>
        )}

        <button disabled={busy} onClick={onConfirm} style={{ width: "100%", marginTop: 16, padding: "17px",
          border: "1.5px solid #000", background: busy ? "#bdbdbd" : "var(--moy)", color: "#fff",
          boxShadow: busy ? "none" : "4px 4px 0 #0B5A33", cursor: busy ? "default" : "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 18, letterSpacing: "0.05em",
          display: "flex", alignItems: "center", justifyContent: "center", gap: 8 }}>
          <EmeGem size={22} /> {busy ? "処理中…" : verb + "する"}
        </button>
        <button disabled={busy} onClick={onCancel} style={{ width: "100%", marginTop: 8, padding: "12px", border: "none",
          background: "transparent", cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)", fontSize: 13,
          fontWeight: 600, color: "var(--ink-soft)" }}>キャンセル</button>
      </div>
    </div>
  );
}

/* ─── エラー表示ヘルパ ───────────────────────────────────────────── */
const ERR_LABEL = {
  insufficient: "残高が不足しています",
  bad_amount: "金額が不正です",
  self_transfer: "自分自身には送れません",
  unknown_target: "相手が見つかりません",
  mc_unavailable: "Minecraft サーバーに接続されていません",
  character_unreachable: "キャラクターに接続できません（ゲーム／サーバー連携をご確認ください）",
  charge_pending: "チャージは保留中です（反映までお待ちください）",
  charge_failed: "チャージに失敗しました。もう一度お試しください",
  withdraw_pending: "出金は保留中です（反映までお待ちください）",
  withdraw_failed: "出金に失敗しました。残高は返金されています",
  withdraw_stuck: "出金の結果を確認できませんでした。運営にお問い合わせください（残高は保留中です）",
  unauthorized: "セッションが切れました。ログインし直してください",
  // ── host attestation (DEV.md §7.3.10 G4) ──
  // The codes the OS/SDK produce keep their own wording — a user who declined, a
  // user on a phone with no host game and a user hitting a packaging bug each
  // need a different sentence, and their remedies differ.
  //
  // The codes the BACKEND produces are deliberately coarser. Which of its policy
  // checks an assertion fell at is not reported: answering differently per reason
  // hands a caller a map of the policy. That detail is in the backend log, and an
  // honest user's remedy ("confirm again") is the same either way.
  attestation_required: "キャラクターの確認が必要です",
  attest_denied: "キャラクターの確認をキャンセルしました",
  attest_timeout: "キャラクターの確認がタイムアウトしました。もう一度お試しください",
  attest_unavailable: "いまキャラクターを確認できません。しばらくしてからもう一度お試しください",
  // Kind-neutral: the same code is reported for a charge and for a withdrawal,
  // and both are unavailable for the same reason (no host game to attest).
  attest_no_host: "この画面では利用できません。Minecraft 内の MochiOS からご利用ください",
  attest_no_scope: "アプリの設定が不足しています（attestation.request）。アプリを更新してください",
  attest_os_error: "MochiOS がキャラクターを確認できませんでした",
  attest_no_crypto: "この環境では確認に必要な暗号機能が使えません",
  attest_challenge: "確認の有効期限が切れました。もう一度お試しください",
  attest_invalid: "キャラクターの確認結果を受け付けられませんでした。もう一度お試しください",
  // ── 段階認証 (riskauth) ──
  // `pin_required` は「聞いてください」、`invalid_pin` は「間違っています」で、
  // まったく別の意味なので文言も分ける。
  // invalid_pin に残り試行回数は出さない: 教えると、あと何回試せるかを
  // 攻撃者に配ることになる。待ち時間だけは本人の行動に必要なので出す。
  pin_required: "PIN を入力してください",
  invalid_pin: "PIN が違います",
  locked: "PIN の入力を一時的に制限しています",
  too_many_attempts: "PIN の入力を一時的に制限しています",
  // ── EC 決済 ──
  unknown_intent: "この支払いは見つかりませんでした（期限切れの可能性があります）",
  payer_mismatch: "この支払いは別の口座あてに発行されています",
  expired: "支払いの期限が切れました。お店でやり直してください",
  already_paid: "この支払いはすでに完了しています",
  not_payable: "この支払いはもう受け付けられません",
  not_declinable: "この支払いはもう取り消せません",
  canceled: "この支払いはお店によって取り消されました",
  declined: "この支払いはキャンセル済みです",
  merchant_disabled: "この加盟店は現在停止中のため支払えません",
  return_failed: "お店のアプリに戻れませんでした。もう一度お試しください",
  network: "通信に失敗しました。もう一度お試しください",
  // ── 加盟店ポータル ──
  bad_name: "この店名は使えません",
  bad_sub: "説明文に使えない文字が含まれています",
  name_taken: "その店名はすでに使われています",
  too_many_merchants: "登録できる加盟店の上限に達しています",
  unknown_merchant: "加盟店が見つかりません",
  bad_status: "指定された状態が不正です",
  email_verification_required: "加盟店の登録にはメールアドレスの確認が必要です",
  rate_limited: "操作が続けて行われました。しばらく待ってからお試しください",
};
function errLabel(code) {
  return ERR_LABEL[code] || "処理に失敗しました（" + (code || "error") + "）";
}

/* 秒数の日本語表記 (待ち時間・残り時間・経過時間で共用)。 */
function moyDuration(ms) {
  const s = Math.max(0, Math.ceil(ms / 1000));
  if (s < 60) return s + "秒";
  const m = Math.floor(s / 60);
  return m + "分" + (s % 60 ? String(s % 60) + "秒" : "");
}

/**
 * サーバの `{ok:false,...}` 応答をそのまま文にする。
 *
 * `retry_after_ms` を持つ応答（locked / too_many_attempts / invalid_pin の
 * バックオフ）は待ち時間を添える。これは試行回数ではなく本人が次に何をすれば
 * よいかの情報なので出してよい。`not_payable` / `not_declinable` は
 * `state` を見て「取り消された」「キャンセル済み」まで言い切る — どちらも
 * 「もう払えません」で片づけると、お店に問い合わせるべきかが分からない。
 */
function errLabelFor(res) {
  if (!res || !res.code) return errLabel(null);
  const stateful = res.code === "not_payable" || res.code === "not_declinable";
  if (stateful && ERR_LABEL[res.state]) return ERR_LABEL[res.state];
  const base = errLabel(res.code);
  if (res.retry_after_ms > 0) return base + "（約" + moyDuration(res.retry_after_ms) + "後にお試しください）";
  return base;
}

/* ─── EC 決済の承認オーバーレイ ───────────────────────────────────
   表示する金額・加盟店・注文内容は、すべて `GET /wallet/payment/intent` の
   応答だけから来る。起動してきたアプリが渡せたのは intent_id ひとつで、
   それ以外は読まずに捨ててある（moymoy-sdk.js の readLaunchIntent）。
   ここが崩れると「見えている内容と請求内容が違う」決済が作れてしまう。 */

const MOY_NEW_MERCHANT_MS = 7 * 24 * 60 * 60 * 1000;

/* 加盟店の顔。@handle を主、店名を副にしてある。
   店名は先取り登録や同形異字で似せられるが、@handle は送金の宛先として
   すでに一意な ID 空間なので、偽装のコストが桁違いに高い。
   見た目の好みではなく、なりすまし加盟店をユーザーが見分けるための序列。 */
function MoyMerchantFace({ face, compact }) {
  const handle = face && face.owner_handle;
  const isNew = face && Date.now() - face.created_unix_ms < MOY_NEW_MERCHANT_MS;
  return (
    <div>
      <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>お支払い先</div>
      {handle ? (
        <div style={{ fontFamily: "var(--font-mono)", fontSize: compact ? 20 : 26, fontWeight: 700,
          color: "var(--ink)", letterSpacing: "-0.01em", marginTop: 2, wordBreak: "break-all" }}>
          @{handle}
        </div>
      ) : (
        // 主表示にできる @handle が無い。店名で埋めると、いちばん偽装しやすい
        // 情報がいちばん強い位置に昇格してしまうので、欠けていることを書く。
        <div style={{ fontFamily: "var(--font-jp)", fontSize: compact ? 15 : 17, fontWeight: 800,
          color: "var(--carle-red)", marginTop: 2 }}>
          所有者を確認できません
        </div>
      )}
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginTop: 4, flexWrap: "wrap" }}>
        <span style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700, color: "var(--ink-soft)" }}>
          {(face && face.name) || "（店名なし）"}
        </span>
        {isNew && (
          <span style={{ border: "1.5px solid var(--carle-red)", color: "var(--carle-red)",
            background: "rgba(227,38,54,0.08)", padding: "1px 6px", fontFamily: "var(--font-jp)",
            fontSize: 10, fontWeight: 700 }}>新規のお店</span>
        )}
      </div>
      {face && face.sub && (
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)", marginTop: 2 }}>
          {face.sub}
        </div>
      )}
    </div>
  );
}

/* 警告バナー。ブロックはしない — Web 決済や複数アプリ構成を殺さないため、
   判断材料を出してユーザーに委ねる。 */
function MoyWarnBanner({ children }) {
  return (
    <div style={{ marginTop: 10, padding: "9px 12px", border: "1.5px solid var(--carle-red)",
      background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
      fontWeight: 700, color: "var(--carle-red)", lineHeight: 1.6 }}>{children}</div>
  );
}

function MoyPaymentApproval({ pending, intent, loading, error, balance, busy, step, done,
  seal, onToPin, onApprove, onDecline, onClose, onCharge, onReturn, onBackToReview }) {
  // 残り時間と経過時間を進めるためだけの時計。
  const [now, setNow] = msState(Date.now());
  msEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const face = intent && intent.merchant;
  const state = intent && intent.state;
  // 支払えない理由。決められるものはここで決めきる — 押しても必ず失敗する
  // ボタンを出すより、なぜ払えないかを書くほうが利用者にできることが増える。
  let blocked = null;
  if (intent) {
    if (state && state !== "created") blocked = state === "paid" ? "already_paid" : state;
    // 加盟店の行そのものが引けない場合、payer_view は merchant:null と
    // merchant_active:false の両方を返す。先に null を見ないと「停止中」と
    // 言ってしまうが、それは分かっていない事実になる。
    else if (!face) blocked = "unknown_intent";
    else if (intent.merchant_active === false) blocked = "merchant_disabled";
  }
  const payable = !!intent && !blocked;
  // 起動元は OS が刻んだ値なので詐称できない。加盟店の自己申告 launch_app_id と
  // 食い違うときは注意を促すが、止めはしない。
  const declared = intent && intent.launch_app_id;
  const originMismatch = !!(declared && pending && pending.from && declared !== pending.from);
  const ageMs = pending ? pending.age_ms + Math.max(0, now - pending.taken_at) : 0;
  const leftMs = intent ? intent.expires_unix_ms - now : 0;
  const amount = intent ? intent.amount : 0;
  // ここでの self_transfer は「加盟店の受取口座が自分の口座」という意味で、
  // 送金画面の「自分自身には送れません」では何が起きたのか伝わらない。
  const errText = !error ? null
    : error.code === "self_transfer" ? "自分のお店には支払えません"
    : errLabelFor(error);

  const summary = intent && (
    <div style={{ border: "1.5px solid var(--ink)", padding: "12px 14px", background: "var(--bg-white)" }}>
      <MoyMerchantFace face={face} compact />
      <div style={{ marginTop: 8, textAlign: "right" }}>
        <Eme amount={amount} size={30} />
      </div>
    </div>
  );

  return (
    <>
      <div className="fade-in" style={{ position: "absolute", inset: 0, zIndex: 150,
        background: "var(--bg-white)", display: "flex", flexDirection: "column" }}>
        {/* header */}
        <div style={{ flexShrink: 0, padding: "56px 18px 14px", background: "var(--moy)", color: "#fff",
          borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
          <div className="paper-noise" style={{ opacity: 0.1 }} />
          <div style={{ position: "relative", zIndex: 1, display: "flex", alignItems: "center", gap: 10 }}>
            <EmeGem size={26} />
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 21,
                letterSpacing: "-0.02em" }}>お支払いの確認</div>
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, opacity: 0.9 }}>MoyMoy</div>
            </div>
            <button onClick={busy ? undefined : onClose} aria-label="close" style={{ flexShrink: 0,
              background: "transparent", border: "1.5px solid rgba(255,255,255,0.7)", color: "#fff",
              width: 32, height: 32, cursor: busy ? "default" : "pointer", display: "grid",
              placeItems: "center", fontFamily: "var(--font-mono)", fontSize: 16, fontWeight: 700,
              padding: 0 }}>×</button>
          </div>
        </div>

        <div style={{ flex: 1, overflow: "auto", padding: "16px 18px 24px" }}>
          {/* 起動元。OS が attest した app_id なので、ここは常に本当のこと。 */}
          <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between",
            gap: 10, fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)",
            letterSpacing: "0.04em", borderBottom: "1px solid rgba(0,0,0,0.12)", paddingBottom: 8 }}>
            <span style={{ minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
              FROM {(pending && pending.from) || "(不明)"}
            </span>
            <span style={{ flexShrink: 0 }}>{moyDuration(ageMs)}前</span>
          </div>

          {loading && (
            <div style={{ padding: "48px 0", textAlign: "center" }}>
              <EmeGem size={44} style={{ margin: "0 auto 14px", opacity: 0.5 }} />
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)" }}>
                お支払い内容を確認しています…
              </div>
            </div>
          )}

          {!loading && !intent && (
            <div style={{ padding: "40px 0", textAlign: "center" }}>
              <EmeGem size={44} style={{ margin: "0 auto 14px", opacity: 0.4 }} />
              <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700,
                color: "var(--carle-red)", lineHeight: 1.7 }}>{errText}</div>
            </div>
          )}

          {!loading && intent && (
            <>
              {originMismatch && (
                <MoyWarnBanner>
                  お店が届け出た起動元（{declared}）と、実際に MoyMoy を開いたアプリ
                  （{pending.from}）が一致しません。心当たりがなければ支払わないでください。
                </MoyWarnBanner>
              )}
              {!blocked && face && !face.owner_handle && (
                <MoyWarnBanner>
                  このお店の所有者アカウントを確認できません。心当たりがなければ
                  支払わないでください。
                </MoyWarnBanner>
              )}

              <div style={{ marginTop: 14 }}>
                <MoyMerchantFace face={face} />
              </div>

              <div style={{ marginTop: 18, textAlign: "center" }}>
                <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>お支払い金額</div>
                <Eme amount={amount} size={52} />
              </div>

              {/* お店が入力した文言。MoyMoy 自身の文章と見分けがつくように引用として
                  枠づけ、高さを固定する。長い説明文で金額表示を画面外へ押し出す
                  細工を、レイアウトの側で成立させないため。 */}
              <div style={{ marginTop: 16 }}>
                <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, letterSpacing: "0.14em",
                  color: "var(--ink-soft)", marginBottom: 4 }}>お店が入力した内容</div>
                <div style={{ padding: "10px 12px", borderLeft: "4px solid var(--ink-soft)",
                  border: "1px dashed rgba(0,0,0,0.3)", borderLeftWidth: 4, borderLeftStyle: "solid",
                  background: "rgba(0,0,0,0.035)", height: 58, overflow: "hidden",
                  fontFamily: "var(--font-jp)", fontSize: 13, lineHeight: 1.45, color: "var(--ink)",
                  display: "-webkit-box", WebkitLineClamp: 3, WebkitBoxOrient: "vertical" }}>
                  {intent.description || "（内容の記載なし）"}
                </div>
              </div>

              <div style={{ marginTop: 16, border: "1.5px solid var(--ink)", padding: "10px 12px",
                fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)" }}>
                <div style={{ display: "flex", justifyContent: "space-between", paddingBottom: 6 }}>
                  <span>支払い後残高</span>
                  <span style={{ color: "var(--ink)", fontWeight: 700 }}>{formatEme(balance - amount)} エメ</span>
                </div>
                {/* 目安。期限を決めるのはサーバの claim ガードで、この端末の
                    時計ではない。だから 0 を切っても「期限切れ」とは言い切らず、
                    支払うボタンもこの値では止めない — 進んだ時計のせいで、まだ
                    払える請求を拒む側に倒れないようにする。 */}
                <div style={{ display: "flex", justifyContent: "space-between", paddingTop: 6,
                  borderTop: "1px solid rgba(0,0,0,0.1)" }}>
                  <span>残り時間（目安）</span>
                  <span style={{ color: leftMs <= 60000 ? "var(--carle-red)" : "var(--ink)", fontWeight: 700 }}>
                    {leftMs > 0 ? moyDuration(leftMs) : "まもなく期限切れ"}
                  </span>
                </div>
              </div>

              {blocked && (
                <div style={{ marginTop: 14, padding: "12px", border: "1.5px solid var(--carle-red)",
                  background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 13,
                  fontWeight: 700, color: "var(--carle-red)", textAlign: "center", lineHeight: 1.6 }}>
                  {errLabel(blocked)}
                </div>
              )}

              {error && (
                <div style={{ marginTop: 12, padding: "10px 12px", border: "1.5px solid var(--carle-red)",
                  background: "rgba(227,38,54,0.08)", fontFamily: "var(--font-jp)", fontSize: 12,
                  fontWeight: 700, color: "var(--carle-red)", textAlign: "center", lineHeight: 1.6 }}>
                  {errText}
                </div>
              )}

              {/* 残高不足。intent はサーバ側で `created` のまま残るので、チャージして
                  同じ intent をもう一度承認できる。だから破棄せず保持したまま
                  チャージ画面へ寄り道させる。 */}
              {error && error.code === "insufficient" && (
                <button onClick={onCharge} style={{ width: "100%", marginTop: 10, padding: "14px",
                  border: "1.5px solid var(--moy-deep)", background: "var(--moy-mint)",
                  color: "var(--moy-deep)", cursor: "pointer", fontFamily: "var(--font-jp)",
                  fontWeight: 800, fontSize: 15 }}>
                  チャージへ（この支払いは保持されます）
                </button>
              )}

              {payable && (
                <button disabled={busy} onClick={onToPin} style={{ width: "100%", marginTop: 18,
                  padding: "17px", border: "1.5px solid #000", background: busy ? "#bdbdbd" : "var(--moy)",
                  color: "#fff", boxShadow: busy ? "none" : "4px 4px 0 #0B5A33",
                  cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)", fontWeight: 800,
                  fontSize: 18, letterSpacing: "0.05em", display: "flex", alignItems: "center",
                  justifyContent: "center", gap: 8 }}>
                  <EmeGem size={22} /> 支払う
                </button>
              )}
              {state === "created" && (
                <button disabled={busy} onClick={onDecline} style={{ width: "100%", marginTop: 8,
                  padding: "14px", border: "1.5px solid var(--ink)", background: "var(--bg-white)",
                  cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)", fontWeight: 700,
                  fontSize: 15, color: "var(--ink)" }}>
                  {busy ? "処理中…" : "支払わない"}
                </button>
              )}
            </>
          )}

          {/* 復帰は OS が刻んだ `from` へ。お店の自己申告を戻り先の権威にしない。 */}
          {pending && pending.from && (
            <button disabled={busy} onClick={onReturn} style={{ width: "100%", marginTop: 10,
              padding: "13px", border: "none", background: "transparent",
              cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)", fontSize: 13,
              fontWeight: 700, color: "var(--moy-deep)" }}>
              ← ショップに戻る
            </button>
          )}
        </div>
      </div>

      {/* 完了パネル。承認直後は CompleteOverlay (zIndex 200) が上に出るので、
          それが閉じたあとにここが見える。 */}
      {done && (
        <div className="fade-in" style={{ position: "absolute", inset: 0, zIndex: 155,
          background: "var(--bg-white)", display: "flex", flexDirection: "column",
          alignItems: "center", justifyContent: "center", padding: "24px 26px", textAlign: "center" }}>
          <EmeGem size={56} style={{ marginBottom: 16 }} />
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 18, fontWeight: 800, color: "var(--ink)" }}>
            {done === "paid" ? "お支払いが完了しました" : "この支払いをキャンセルしました"}
          </div>
          {done === "paid" && intent && (
            <div style={{ marginTop: 12 }}><Eme amount={intent.amount} size={34} /></div>
          )}
          <div style={{ marginTop: 10, fontFamily: "var(--font-jp)", fontSize: 13,
            color: "var(--ink-soft)", lineHeight: 1.8 }}>
            {done === "paid"
              ? "お店のアプリに戻ると、商品の受け取りに進めます。"
              : "お店のアプリに戻ってやり直せます。"}
          </div>
          {error && (
            <div style={{ marginTop: 12, fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700,
              color: "var(--carle-red)" }}>{errText}</div>
          )}
          {pending && pending.from ? (
            <button disabled={busy} onClick={onReturn} style={{ width: "100%", maxWidth: 320,
              marginTop: 22, padding: "16px", border: "1.5px solid #000",
              background: busy ? "#bdbdbd" : "var(--moy)", color: "#fff",
              boxShadow: busy ? "none" : "4px 4px 0 #0B5A33", cursor: busy ? "default" : "pointer",
              fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17 }}>
              {busy ? "戻っています…" : "ショップに戻る"}
            </button>
          ) : null}
          <button disabled={busy} onClick={onClose} style={{ marginTop: 10, border: "none",
            background: "transparent", cursor: busy ? "default" : "pointer",
            fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>
            ウォレットに戻る
          </button>
        </div>
      )}

      {step === "pin" && (
        <MoyPinStep zIndex={160} title="PIN で承認" sub="この支払いを実行します"
          summary={summary} showSeal seal={seal}
          notice="MoyMoy は決済画面以外で PIN を求めません。"
          busy={busy} error={errText} cta="支払う"
          onCancel={onBackToReview} onSubmit={onApprove} />
      )}
    </>
  );
}

/* 支払いを保持したままチャージへ寄り道しているときのバー。 */
function MoyPaymentResumeBar({ onResume }) {
  return (
    <div style={{ position: "absolute", left: 0, right: 0, bottom: 92, zIndex: 70, padding: "0 16px" }}>
      <button onClick={onResume} style={{ width: "100%", padding: "13px 14px",
        border: "1.5px solid #000", background: "var(--moy-deep)", color: "#fff",
        boxShadow: "3px 3px 0 #063C21", cursor: "pointer", display: "flex", alignItems: "center",
        justifyContent: "center", gap: 8, fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 14 }}>
        <EmeGem size={18} /> 支払い待ちの取引があります · 承認画面へ戻る
      </button>
    </div>
  );
}

/* ─── オーケストレータ (moymoy.cs.mnn 配線) ─────────────────────── */
function MoyMoyApp({ onClose, account, accounts = [], onSwitchAccount, onAddAccount, onLogoutAccount,
  pendingPayment, onPaymentDone }) {
  const [balance, setBalance] = msState(0);
  const [profile, setProfile] = msState({ holder: "PLAYER", number: "•••• •••• •••• ••••", expiry: "07/29" });
  const [txns, setTxns] = msState([]);
  const [friends, setFriends] = msState([]);
  const [inv, setInv] = msState({ emeralds: 0, blocks: 0 });
  const [invStatus, setInvStatus] = msState(null); // null = ok; else the inventory error code
  const [attesting, setAttesting] = msState(false); // a consent modal is up
  const [canCharge, setCanCharge] = msState(false);
  const [loaded, setLoaded] = msState(false);
  const mountedRef = msRef(true);
  // A stable idem_key for the in-flight transaction (send/pay/charge). A retry
  // after a network error or poll timeout reuses it so the server replays the
  // same op instead of executing twice (no double send/pay/charge).
  const idemRef = msRef(null);

  const [tab, setTab] = msState("home");
  const [emeMode, setEmeMode] = msState("charge"); // "charge" | "withdraw" — segment on the charge tab
  const [flow, setFlow] = msState(null);       // {kind, target}  amount-entry
  const [confirm, setConfirm] = msState(null);  // {kind, target, amount}
  const [busy, setBusy] = msState(false);
  const [err, setErr] = msState(null);
  const [complete, setComplete] = msState(null);
  const [menuOpen, setMenuOpen] = msState(false);
  const [settingsOpen, setSettingsOpen] = msState(false);
  const [portalOpen, setPortalOpen] = msState(false);

  // 送金・出金の step-up PIN。承認画面の PIN ステップと同じ MoyPinStep を使う。
  const [pinAsk, setPinAsk] = msState(false);
  const [pinErr, setPinErr] = msState(null);

  // 個人化シール（口座ごと）。PIN 画面が開く前に読んでおく — あとから
  // 遅れて現れるシールは「出ていないこと」を手がかりにできなくなる。
  const [seal, setSeal] = msState(null);
  const [sealSetup, setSealSetup] = msState(false);

  // ── EC 決済の承認 ──
  const [payIntent, setPayIntent] = msState(null);   // payer_view | null
  const [payLoading, setPayLoading] = msState(false);
  const [payErr, setPayErr] = msState(null);         // {code, state, retry_after_ms} | null
  const [payStep, setPayStep] = msState("review");   // review | pin
  const [payDone, setPayDone] = msState(null);       // "paid" | "declined" | null
  const [payBusy, setPayBusy] = msState(false);
  const [payMin, setPayMin] = msState(false);        // チャージへ寄り道中

  // A session that expired mid-use ⇒ drop this account and re-authenticate.
  function onExpired() {
    if (account && onLogoutAccount) onLogoutAccount(account.account_id);
  }

  // MoyHome's quick actions pass tab ids, but "withdraw" isn't a tab — it's a
  // segment inside "charge". MoyHome doesn't need to know that: it just asks to
  // go somewhere, and this is the one place that interprets what "somewhere"
  // means, keeping that mapping out of the presentational component.
  function goFromHome(id) {
    if (id === "withdraw") { setEmeMode("withdraw"); setTab("charge"); return; }
    // Any other quick action (charge included) lands on its own tab in charge
    // mode — otherwise a "チャージ" tap right after a withdraw would silently
    // reopen the withdraw segment instead.
    setEmeMode("charge");
    setTab(id);
  }

  const rootStyle = {
    "--moy": "#16A35A",
    "--moy-deep": "#0B7A41",
    "--moy-light": "#2ECC71",
    "--moy-mint": "#D6F5E3",
  };

  async function refresh(isAlive = () => true) {
    try {
      const h = await MoyMoy.home();
      if (!isAlive()) return;
      if (h.ok) {
        setBalance(h.balance);
        setProfile(h.profile);
        setTxns(mapTxns(h.txns));
        setCanCharge(!!h.can_charge);
      } else if (h.error === "unauthorized") {
        onExpired();
      }
    } catch (e) { console.warn("MoyMoy: home refresh failed — showing last known state", e); }
  }

  // 加盟店一覧はもう読まない: `/wallet/pay` の廃止で一覧から支払う導線が無く
  // なり、`wallet::merchants()` も `listed = 1` の店しか返さない（自己登録した
  // 店は既定で 0）。押しても何も起きない一覧のために、開くたび往復するのをやめる。
  async function loadAll(isAlive = () => true) {
    await refresh(isAlive);
    try {
      const f = await MoyMoy.friends();
      if (isAlive() && f.ok) setFriends(f.friends.map(enrichFriend));
    } catch (e) { console.warn("MoyMoy: friends load failed", e); }
    if (isAlive()) setLoaded(true);
  }

  // Inventory is NOT part of loadAll: it is only meaningful on the charge tab,
  // and reading it on open would report `attestation_required` (and show the
  // confirm prompt) the moment the wallet appears, for a user who came to check
  // their balance.
  async function refreshInventory(isAlive = () => true) {
    try {
      const i = await MoyMoy.inventory();
      if (!isAlive()) return;
      if (i.ok) { setInv({ emeralds: i.emeralds, blocks: i.blocks }); setInvStatus(null); }
      else { setInv({ emeralds: 0, blocks: 0 }); setInvStatus(i.error || "inventory_error"); }
    } catch (e) {
      console.warn("MoyMoy: inventory load failed", e);
      if (isAlive()) setInvStatus("inventory_error");
    }
  }

  // Ask the OS to confirm which character is playing. User-initiated only — this
  // raises a consent modal.
  async function confirmCharacter() {
    if (attesting) return;
    setAttesting(true);
    try {
      const r = await MoyMoy.attestSession();
      if (!mountedRef.current) return;
      if (r.error === "unauthorized") { onExpired(); return; }
      if (!r.ok) { setInvStatus(r.error || "attest_os_error"); return; }
      await refreshInventory(() => mountedRef.current);
    } catch (e) {
      console.warn("MoyMoy: character confirmation failed", e);
      if (mountedRef.current) setInvStatus("attest_os_error");
    } finally {
      if (mountedRef.current) setAttesting(false);
    }
  }

  msEffect(() => {
    mountedRef.current = true;
    let alive = true;
    loadAll(() => alive);
    return () => { alive = false; mountedRef.current = false; };
  }, []);

  // シールは口座に紐づく。口座を切り替えたら読み直す。
  msEffect(() => {
    if (!account) { setSeal(null); return; }
    let alive = true;
    loadSeal(account.account_id)
      .then((s) => { if (alive) setSeal(s); })
      .catch((e) => console.warn("MoyMoy: seal load failed", e));
    return () => { alive = false; };
  }, [account && account.account_id]);

  async function persistSeal(next) {
    if (!account) return false;
    const ok = await saveSeal(account.account_id, next);
    if (ok && mountedRef.current) setSeal(next);
    return ok;
  }

  /**
   * 承認画面が表示するものを取りに行く。
   *
   * 起動元アプリが渡せたのは intent_id だけで、金額も店名も注文内容も
   * この応答からしか読まない。ここを他の入力で補完すると WYSIWYS が崩れる。
   */
  async function loadPaymentIntent(intentId, isAlive = () => true) {
    try {
      const r = await MoyMoy.paymentIntent(intentId);
      if (!isAlive()) return;
      if (r.ok && r.intent) { setPayIntent(r.intent); setPayErr(null); }
      else if (r.error === "unauthorized") { onExpired(); return; }
      else setPayErr({ code: r.error || "unknown_intent" });
    } catch (e) {
      console.warn("MoyMoy: payment intent load failed", e);
      if (isAlive()) setPayErr({ code: "network" });
    }
    if (isAlive()) setPayLoading(false);
  }

  // 新しい intent が届いたら承認画面を初期状態から開き直す。
  msEffect(() => {
    const id = pendingPayment && pendingPayment.intent_id;
    if (!id) return;
    let alive = true;
    setPayIntent(null); setPayErr(null); setPayDone(null);
    setPayStep("review"); setPayMin(false); setPayBusy(false); setPayLoading(true);
    loadPaymentIntent(id, () => alive);
    return () => { alive = false; };
  }, [pendingPayment && pendingPayment.intent_id]);

  function clearPayment() {
    setPayIntent(null); setPayErr(null); setPayDone(null);
    setPayStep("review"); setPayMin(false); setPayBusy(false); setPayLoading(false);
    if (onPaymentDone) onPaymentDone();
  }

  /**
   * 承認して支払う。
   *
   * `idem_key` は作らない。intent の行そのものが冪等の記録で、応答を取り逃した
   * リトライは同じ intent_id を投げ直せばサーバが同じ答えを組み立て直す。
   * ここで鍵を作ると「同じ支払い」の定義が二重になる。
   */
  async function doApprove(pin) {
    if (!pendingPayment || payBusy) return;
    setPayBusy(true); setPayErr(null);
    try {
      const r = await MoyMoy.paymentApprove(pendingPayment.intent_id, pin);
      if (!mountedRef.current) return;
      if (r.ok) {
        await refresh(() => mountedRef.current);
        if (!mountedRef.current) return;
        setPayBusy(false);
        setPayStep("review");
        setPayDone("paid");
        setComplete({ kind: "pay", target: r.merchant_name || null, amount: r.amount });
        return;
      }
      if (r.error === "unauthorized") { setPayBusy(false); onExpired(); return; }
      // PIN の打ち直しで解決するものだけ PIN 画面に留める。それ以外は
      // 内容確認に戻す — 期限切れや加盟店停止は、PIN を入れ直しても変わらない。
      if (r.error !== "invalid_pin") setPayStep("review");
      setPayBusy(false);

      // サーバ側で状態が変わっているもの。手元の payer_view は古いので取り直す
      // — そうしないと、もう払えない intent に「支払う」ボタンが出たままになる。
      // 理由は取り直した `state` から描かれるので、ここでエラーは持たない。
      // `payer_mismatch` もここに入る: 取り直すと GET も同じ理由で拒むので
      // payer_view が得られず、「支払う」が消えて理由だけが残る。外していると
      // ボタンが出たままになり、サーバが必ず弾く操作をユーザーに繰り返させる。
      const stale = ["expired", "already_paid", "not_payable", "merchant_disabled", "payer_mismatch"]
        .indexOf(r.error) >= 0;
      if (stale) {
        setPayErr(null); setPayIntent(null); setPayLoading(true);
        await loadPaymentIntent(pendingPayment.intent_id, () => mountedRef.current);
        return;
      }
      setPayErr({ code: r.error, state: r.state, retry_after_ms: r.retry_after_ms });
      // 残高不足なら残高表示を最新にしておく（チャージ導線の判断材料になる）。
      if (r.error === "insufficient") await refresh(() => mountedRef.current);
    } catch (e) {
      // 通信断。同じ intent_id で押し直せばよいので、PIN 画面のまま残す。
      console.warn("MoyMoy: payment approve failed", e);
      if (!mountedRef.current) return;
      setPayErr({ code: "network" }); setPayBusy(false);
    }
  }

  async function doDecline() {
    if (!pendingPayment || payBusy) return;
    setPayBusy(true); setPayErr(null);
    try {
      const r = await MoyMoy.paymentDecline(pendingPayment.intent_id);
      if (!mountedRef.current) return;
      if (r.ok) setPayDone("declined");
      else if (r.error === "unauthorized") { setPayBusy(false); onExpired(); return; }
      else setPayErr({ code: r.error, state: r.state });
      setPayBusy(false);
    } catch (e) {
      console.warn("MoyMoy: payment decline failed", e);
      if (!mountedRef.current) return;
      setPayErr({ code: "network" }); setPayBusy(false);
    }
  }

  /**
   * 起動してきたアプリへ戻る。
   *
   * 戻り先は OS が刻んだ `from` だけを使う。加盟店が intent に自己申告した
   * `launch_app_id` を戻り先にすると、お店が「支払い後にユーザーをどこへ
   * 送るか」を決められることになり、偽の完了画面へ誘導する余地ができる。
   */
  async function returnToShop() {
    const from = pendingPayment && pendingPayment.from;
    if (!from) { clearPayment(); return; }
    setPayBusy(true); setPayErr(null);
    const r = await MoyMoy.launchApp(from);
    if (!mountedRef.current) return;
    setPayBusy(false);
    if (r && r.accepted) { clearPayment(); return; }
    // 戻れない理由（`not_foreground` / `rate_limited` / `not_installed`）は
    // 押し直しや手動復帰で対処できるので、握り潰さず出す。
    console.warn("MoyMoy: return to " + from + " refused", r);
    setPayErr({ code: "return_failed", reason: r && r.reason });
  }

  // 残高不足のときの寄り道。intent はサーバ側で `created` のまま残るので、
  // 破棄せず保持したままチャージへ行き、戻ってきて同じ intent を承認できる。
  function payGoCharge() {
    setPayMin(true);
    setEmeMode("charge");
    setTab("charge");
  }
  // 戻ってきたら取り直す。チャージと op のポーリングで時間が経っており、
  // 手元の payer_view の残り時間も状態も古くなっている可能性がある。
  function payResume() {
    if (!pendingPayment) return;
    setPayMin(false); setPayStep("review"); setPayErr(null);
    setPayIntent(null); setPayLoading(true);
    loadPaymentIntent(pendingPayment.intent_id, () => mountedRef.current);
  }

  // Read the inventory when the charge tab opens (and on every return to it, so
  // a character who moved servers is re-detected).
  msEffect(() => {
    if (tab !== "charge") return;
    let alive = true;
    refreshInventory(() => alive);
    return () => { alive = false; };
  }, [tab]);

  // Full history (separate from the home "recent" slice) when the history tab opens.
  msEffect(() => {
    if (tab !== "history") return;
    let alive = true;
    MoyMoy.history("all", 100).then(r => { if (alive && r.ok) setTxns(mapTxns(r.txns)); }).catch(e => console.warn("MoyMoy: history load failed", e));
    return () => { alive = false; };
  }, [tab]);

  async function pollOp(opId, tries = 25) {
    let lastErr = null;
    for (let i = 0; i < tries; i++) {
      try {
        const r = await MoyMoy.op(opId);
        if (r.error === "unauthorized") return { unauthorized: true };
        if (r.ok && r.op && (r.op.state === "settled" || r.op.state === "failed" || r.op.state === "stuck")) return r.op;
      } catch (e) { lastErr = e; /* transient — keep polling */ }
      if (i < tries - 1) await new Promise(res => setTimeout(res, 600));
    }
    if (lastErr) console.warn("MoyMoy: pollOp gave up after " + tries + " attempts", lastErr);
    return null;
  }

  /**
   * 確認シートの実行。`pin` は riskauth が要求したときだけ渡る。
   *
   * 最初は PIN 無しで投げる: 何が必要かを決めるのはサーバ側の riskauth で、
   * 金額の小さい送金まで PIN を取ると、閾値を設けた意味が無くなる。
   * `pin_required` が返ったら PIN を集めて、**同じ idem_key で**投げ直す。
   */
  async function doConfirm(pin) {
    if (!confirm || busy) return;
    const { kind, target, amount } = confirm;
    setBusy(true);
    setErr(null);
    try {
      let res;
      // Reuse one idem_key across retries of this attempt so a retry (after a
      // network error or poll timeout) replays the same op, never executing twice.
      if (!idemRef.current) idemRef.current = MoyMoy.newIdem();
      if (kind === "send") {
        res = await MoyMoy.send(target.handle, amount, idemRef.current, pin);
      } else if (kind === "withdraw") {
        res = await MoyMoy.withdraw(amount, idemRef.current, pin);
      } else {
        // チャージは資金の流入なので riskauth の対象外 — PIN は渡さない。
        res = await MoyMoy.charge(amount, idemRef.current);
      }

      if (!res.ok) {
        if (res.error === "unauthorized") { setBusy(false); setConfirm(null); onExpired(); return; }
        if (res.error === "pin_required") {
          setPinErr(null); setPinAsk(true); setBusy(false);
          return;
        }
        if (res.error === "invalid_pin" || res.error === "locked" || res.error === "too_many_attempts") {
          // PIN の打ち直しで解決するので、確認シートには戻さず PIN 画面に留める。
          setPinErr({ code: res.error, retry_after_ms: res.retry_after_ms });
          setPinAsk(true); setBusy(false);
          return;
        }
        setErr(errLabelFor({ code: res.error, state: res.state, retry_after_ms: res.retry_after_ms }));
        setPinAsk(false);
        setBusy(false);
        return;
      }
      setPinAsk(false); setPinErr(null);

      let shownAmount = amount;
      if (kind === "charge" || kind === "withdraw") {
        // Charge/withdraw are async: poll the op until the mod settles the amount.
        const op = await pollOp(res.op_id);
        if (op && op.unauthorized) { setBusy(false); setConfirm(null); onExpired(); return; }
        if (!op || op.state !== "settled") {
          if (!mountedRef.current) return;
          if (kind === "charge") {
            const terminal = op && (op.state === "failed" || op.state === "stuck");
            // Terminal failure ⇒ this op is dead; let a retry start a fresh op.
            if (terminal) idemRef.current = null;
            setErr(errLabel(terminal ? "charge_failed" : "charge_pending"));
          } else if (op && op.state === "failed") {
            // A failed withdrawal is refunded server-side and dead — a retry must
            // start a fresh op.
            idemRef.current = null;
            setErr(errLabel("withdraw_failed"));
          } else if (op && op.state === "stuck") {
            // Unlike a failure, "stuck" means the outcome is unknown — it may yet
            // settle — so keep pointing at the SAME op instead of risking a second
            // withdrawal alongside a first whose result we can't confirm.
            setErr(errLabel("withdraw_stuck"));
          } else {
            setErr(errLabel("withdraw_pending"));
          }
          setBusy(false);
          await refresh(() => mountedRef.current);
          return;
        }
        shownAmount = op.settled_amount != null ? op.settled_amount : amount;
      }

      await refresh(() => mountedRef.current);
      if (!mountedRef.current) return;
      // Only a charge or withdrawal changes the in-world inventory; re-reading it
      // after a send or a payment would be a pointless round-trip to the game server.
      if (kind === "charge" || kind === "withdraw") {
        await refreshInventory(() => mountedRef.current);
      }
      if (!mountedRef.current) return;

      idemRef.current = null; // settled — the next transaction starts a fresh op
      setBusy(false);
      setConfirm(null);
      setFlow(null);
      setComplete({ kind, target: target ? target.name : null, amount: shownAmount });
      setTab("home");
    } catch (e) {
      console.warn("MoyMoy: confirm (" + kind + ") failed", e);
      if (!mountedRef.current) return;
      setErr("通信に失敗しました");
      setBusy(false);
    }
  }

  return (
    <div className="screen fade-in" style={{ position: "absolute", inset: 0, zIndex: 20,
      background: "var(--bg-white)", display: "flex", flexDirection: "column", ...rootStyle }}>
      <MoyHeader onClose={onClose} account={account} onMenu={() => setMenuOpen(true)} />

      <div style={{ flex: 1, overflow: "auto", position: "relative" }}>
        {tab === "home" && <MoyHome balance={balance} txns={txns} profile={profile} onTab={goFromHome} />}
        {tab === "pay" && <MoyPay txns={txns} />}
        {tab === "send" && <MoySend friends={friends} onPick={t => setFlow({ kind: "send", target: t })} />}
        {tab === "charge" && (
          <>
            <MoyEmeModeSwitch mode={emeMode} onMode={setEmeMode} />
            {emeMode === "charge"
              ? <MoyCharge balance={balance} inv={inv} canCharge={canCharge} invStatus={invStatus}
                  attesting={attesting} onConfirmCharacter={confirmCharacter}
                  onConfirm={(amount) => { setErr(null); setConfirm({ kind: "charge", target: null, amount }); }} />
              : <MoyWithdraw balance={balance} canCharge={canCharge} invStatus={invStatus}
                  attesting={attesting} onConfirmCharacter={confirmCharacter}
                  onConfirm={(amount) => { setErr(null); setConfirm({ kind: "withdraw", target: null, amount }); }} />}
          </>
        )}
        {tab === "history" && <MoyHistory txns={txns} />}
      </div>

      <MoyBottomNav tab={tab} onTab={t => {
        // The bottom nav's charge button is labeled "チャージ" too — same promise
        // as the home quick action, so it gets the same reset (see goFromHome).
        if (t === "charge") setEmeMode("charge");
        setTab(t); setFlow(null);
      }} />

      {/* amount entry */}
      {flow && (
        <MoyAmountEntry kind={flow.kind} target={flow.target} balance={balance}
          onCancel={() => setFlow(null)}
          onNext={amount => { setErr(null); setConfirm({ kind: flow.kind, target: flow.target, amount }); }} />
      )}

      {/* confirm sheet */}
      {confirm && (
        <MoyConfirmSheet {...confirm} balance={balance} busy={busy} error={err}
          onCancel={() => { idemRef.current = null; setConfirm(null); setErr(null); setPinAsk(false); setPinErr(null); }}
          onConfirm={() => doConfirm()} />
      )}

      {/* riskauth の step-up。承認画面と同じ MoyPinStep を使う — PIN を集める
          画面が 2 つに分かれると、片方だけがシールと注意文を失う。 */}
      {confirm && pinAsk && (
        <MoyPinStep zIndex={150}
          title="PIN で確認" sub={confirm.kind === "withdraw" ? "この出金を実行します" : "この送金を実行します"}
          summary={
            <div style={{ border: "1.5px solid var(--ink)", padding: "12px 14px" }}>
              {confirm.target && (
                <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 700 }}>
                  {confirm.target.name}
                </div>
              )}
              <div style={{ marginTop: 6, textAlign: "right" }}><Eme amount={confirm.amount} size={30} /></div>
            </div>
          }
          showSeal seal={seal}
          notice="MoyMoy は決済画面以外で PIN を求めません。"
          busy={busy} error={pinErr ? errLabelFor(pinErr) : null} cta="実行する"
          onCancel={() => { setPinAsk(false); setPinErr(null); }}
          onSubmit={(pin) => doConfirm(pin)} />
      )}

      {/* EC 決済の承認。zIndex 150 — 確認シート(110)より上、完了演出(200)より下。 */}
      {pendingPayment && !payMin && (
        <MoyPaymentApproval
          pending={pendingPayment} intent={payIntent} loading={payLoading} error={payErr}
          balance={balance} busy={payBusy} step={payStep} done={payDone}
          seal={seal}
          onToPin={() => { setPayErr(null); setPayStep("pin"); }}
          onBackToReview={() => { setPayErr(null); setPayStep("review"); }}
          onApprove={doApprove} onDecline={doDecline}
          onClose={clearPayment} onCharge={payGoCharge} onReturn={returnToShop} />
      )}
      {pendingPayment && payMin && <MoyPaymentResumeBar onResume={payResume} />}

      {/* complete */}
      {complete && (
        <CompleteOverlay {...complete} sound onClose={() => setComplete(null)} />
      )}

      {/* account switcher + settings */}
      <AccountMenu open={menuOpen} accounts={accounts} activeId={account ? account.account_id : null}
        onSwitch={(id) => { setMenuOpen(false); if (onSwitchAccount) onSwitchAccount(id); }}
        onAdd={() => { setMenuOpen(false); if (onAddAccount) onAddAccount(); }}
        onLogout={(id) => { setMenuOpen(false); if (onLogoutAccount) onLogoutAccount(id); }}
        onSettings={() => { setMenuOpen(false); setSettingsOpen(true); }}
        onClose={() => setMenuOpen(false)} />
      <SettingsSheet open={settingsOpen} account={account} seal={seal}
        onLogout={() => { setSettingsOpen(false); if (account && onLogoutAccount) onLogoutAccount(account.account_id); }}
        onMerchant={() => { setSettingsOpen(false); setPortalOpen(true); }}
        onSeal={() => { setSettingsOpen(false); setSealSetup(true); }}
        onClose={() => setSettingsOpen(false)} />
      {/* 開くたびに新しくマウントする。据え置くと、シールを設定したあとに
          開き直しても入力欄が初回マウント時の値のままになる。 */}
      {portalOpen && <MerchantPortal open onClose={() => setPortalOpen(false)} />}
      {sealSetup && <MoySealSetup open seal={seal} onSave={persistSeal} onClose={() => setSealSetup(false)} />}
    </div>
  );
}

Object.assign(window, {
  MoyKeypad, MineSlot, EmeBlockMini, MoyPay, MoySend, MoyAmountEntry, MoyEmeModeSwitch,
  MoyCharge, MoyWithdraw, MoyHistory, MoyConfirmSheet, MoyMoyApp,
  MoyPinStep, MoySealBadge, MoySealSetup, MoyPaymentApproval, MoyPaymentResumeBar,
  errLabel, errLabelFor,
});
