/* global React, CrystalIcon, EmeGem, Eme, formatEme, MoyMoyCard, MoyHeader, MoyBottomNav, MoyHome, CompleteOverlay, MoyMoy */
/* =====================================================================
   MoyMoy — 画面: 支払う(近接) / 送る / チャージ / 履歴
            + キーパッド / 金額入力 / 確認シート / オーケストレータ
   Adapted from Claude Design "MochiOS Mobile.html": the presentational layer
   is unchanged; MoyPay/MoySend take their list as a prop, and MoyMoyApp is wired
   to the moymoy.cs.mnn backend (MoyMoy SDK) instead of local mock data.
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
function enrichMerchant(m) {
  return { ...m, pal: m.pal || "emerald", glyph: m.glyph || "◈" };
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

/* ─── 支払う (近接スキャン) ─────────────────────────────────────── */
function MoyPay({ merchants = [], onPick }) {
  return (
    <div style={{ padding: "0 0 120px" }}>
      {/* radar */}
      <div style={{ position: "relative", height: 168, background: "var(--moy)", overflow: "hidden",
        borderBottom: "1.5px solid var(--ink)", display: "grid", placeItems: "center" }}>
        <div className="paper-noise" style={{ opacity: 0.1 }} />
        {[1,2,3].map(r => (
          <div key={r} style={{ position: "absolute", width: 60 * r, height: 60 * r,
            border: "1.5px solid rgba(255,255,255,0.45)", borderRadius: "50%",
            animation: `moy-ping 2.4s ${r * 0.4}s ease-out infinite` }} />
        ))}
        <div style={{ position: "relative", zIndex: 1, textAlign: "center", color: "#fff" }}>
          <EmeGem size={40} style={{ margin: "0 auto" }} />
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, marginTop: 8,
            textShadow: "0 1px 2px rgba(0,0,0,0.3)" }}>近くのお店・プレイヤーを検出中</div>
        </div>
      </div>
      <div style={{ padding: "14px 16px 8px" }}>
        <div className="h-section">加盟店 · {merchants.length}件</div>
      </div>
      <div style={{ borderTop: "1.5px solid var(--ink)", borderBottom: "1.5px solid var(--ink)" }}>
        {merchants.map(m => (
          <TargetRow key={m.id} item={m} onClick={() => onPick(m)}
            trailing={m.dist ? <span style={{ fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700,
              color: "var(--moy-deep)", letterSpacing: "0.04em" }}>{m.dist}</span> : null} />
        ))}
        {merchants.length === 0 && (
          <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
            fontSize: 13, color: "var(--ink-soft)" }}>加盟店がありません</div>
        )}
      </div>
      <div style={{ padding: "16px", fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)",
        lineHeight: 1.6, textAlign: "center" }}>
        お店の名前をタップして金額を入力し、お支払いください。
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
};
function errLabel(code) {
  return ERR_LABEL[code] || "処理に失敗しました（" + (code || "error") + "）";
}

/* ─── オーケストレータ (moymoy.cs.mnn 配線) ─────────────────────── */
function MoyMoyApp({ onClose, account, accounts = [], onSwitchAccount, onAddAccount, onLogoutAccount }) {
  const [balance, setBalance] = msState(0);
  const [profile, setProfile] = msState({ holder: "PLAYER", number: "•••• •••• •••• ••••", expiry: "07/29" });
  const [txns, setTxns] = msState([]);
  const [merchants, setMerchants] = msState([]);
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

  // A session that expired mid-use ⇒ drop this account and re-authenticate.
  function onExpired() {
    if (account && onLogoutAccount) onLogoutAccount(account.account_id);
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

  async function loadAll(isAlive = () => true) {
    await refresh(isAlive);
    try {
      const m = await MoyMoy.merchants();
      if (isAlive() && m.ok) setMerchants(m.merchants.map(enrichMerchant));
    } catch (e) { console.warn("MoyMoy: merchants load failed", e); }
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

  async function doConfirm() {
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
        res = await MoyMoy.send(target.handle, amount, idemRef.current);
      } else if (kind === "pay") {
        res = await MoyMoy.pay(target.id, amount, idemRef.current);
      } else if (kind === "withdraw") {
        res = await MoyMoy.withdraw(amount, idemRef.current);
      } else {
        res = await MoyMoy.charge(amount, idemRef.current);
      }

      if (!res.ok) {
        if (res.error === "unauthorized") { setBusy(false); setConfirm(null); onExpired(); return; }
        setErr(errLabel(res.error));
        setBusy(false);
        return;
      }

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
        {tab === "home" && <MoyHome balance={balance} txns={txns} profile={profile} onTab={setTab} />}
        {tab === "pay" && <MoyPay merchants={merchants} onPick={t => setFlow({ kind: "pay", target: t })} />}
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

      <MoyBottomNav tab={tab} onTab={t => { setTab(t); setFlow(null); }} />

      {/* amount entry */}
      {flow && (
        <MoyAmountEntry kind={flow.kind} target={flow.target} balance={balance}
          onCancel={() => setFlow(null)}
          onNext={amount => { setErr(null); setConfirm({ kind: flow.kind, target: flow.target, amount }); }} />
      )}

      {/* confirm sheet */}
      {confirm && (
        <MoyConfirmSheet {...confirm} balance={balance} busy={busy} error={err}
          onCancel={() => { idemRef.current = null; setConfirm(null); setErr(null); }}
          onConfirm={doConfirm} />
      )}

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
      <SettingsSheet open={settingsOpen} account={account}
        onLogout={() => { setSettingsOpen(false); if (account && onLogoutAccount) onLogoutAccount(account.account_id); }}
        onClose={() => setSettingsOpen(false)} />
    </div>
  );
}

Object.assign(window, {
  MoyKeypad, MineSlot, EmeBlockMini, MoyPay, MoySend, MoyAmountEntry, MoyEmeModeSwitch,
  MoyCharge, MoyWithdraw, MoyHistory, MoyConfirmSheet, MoyMoyApp,
});
