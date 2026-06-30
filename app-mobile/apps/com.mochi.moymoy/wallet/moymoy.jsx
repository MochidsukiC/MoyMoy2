/* global React, CrystalIcon */
/* =====================================================================
   MoyMoy — ゲーム内エメラルド決済 (PayPay × クレジットカード)
   Core: 宝石グリフ / エメラルドブロック質感カード / 残高ホーム /
         ボトムナビ / モイモイ♪ 決済完了演出 (Web Audio)
   (verbatim presentational layer from Claude Design "MochiOS Mobile.html")
   ===================================================================== */

const { useState: mmState, useEffect: mmEffect, useRef: mmRef } = React;

/* ─── 通貨ヘルパ ─────────────────────────────────────────────────── */
function formatEme(n) {
  const neg = n < 0;
  const s = Math.abs(Math.round(n)).toLocaleString("en-US");
  return (neg ? "−" : "") + s;
}
// 9エメ = 1ブロック (Minecraft)
function toBlocks(n) {
  const b = Math.floor(Math.abs(n) / 9);
  return b;
}

/* ─── エメラルド宝石マーク (ロゴ / 通貨記号) ───────────────────────── */
function EmeGem({ size = 24, style, id = "eg" }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" style={{ display: "block", ...style }}>
      {/* outer emerald-cut octagon */}
      <polygon points="11,2 21,2 30,11 30,21 21,30 11,30 2,21 2,11" fill="#0B7A41" />
      <polygon points="11.5,3 20.5,3 29,11.5 29,20.5 20.5,29 11.5,29 3,20.5 3,11.5" fill="#1B9E54" />
      {/* crown facets */}
      <polygon points="11.5,3 20.5,3 24.5,9 7.5,9" fill="#3FD981" />
      <polygon points="11.5,29 20.5,29 24.5,23 7.5,23" fill="#0E8A47" />
      <polygon points="3,11.5 7.5,9 7.5,23 3,20.5" fill="#16A35A" />
      <polygon points="29,11.5 24.5,9 24.5,23 29,20.5" fill="#127D43" />
      {/* table */}
      <polygon points="7.5,9 24.5,9 24.5,23 7.5,23" fill="#2ECC71" />
      <polygon points="7.5,9 24.5,9 16,16" fill="#5CEB95" />
      <polygon points="7.5,9 16,16 7.5,23" fill="#3FD981" />
      {/* sparkle */}
      <polygon points="11,11 14.5,11 13,13.5" fill="#D6FFE8" opacity="0.9" />
    </svg>
  );
}

/* ─── 通貨表示  ◈ 12,480 エメ ───────────────────────────────────── */
function Eme({ amount, size = 40, weight = 800, gem = true, suffix = true, color, sign = false, style }) {
  const neg = amount < 0;
  const txt = (sign ? (neg ? "−" : "+") : "") + formatEme(amount);
  return (
    <span style={{ display: "inline-flex", alignItems: "baseline", gap: size * 0.16,
      color: color || "inherit", lineHeight: 1, ...style }}>
      {gem && <EmeGem size={size * 0.62} style={{ alignSelf: "center", transform: "translateY(2%)" }} />}
      <span style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: weight,
        fontSize: size, letterSpacing: "-0.03em", fontVariantNumeric: "tabular-nums" }}>{txt}</span>
      {suffix && <span style={{ fontFamily: "var(--font-jp)", fontWeight: 700,
        fontSize: size * 0.34, letterSpacing: "0.02em", opacity: 0.8 }}>エメ</span>}
    </span>
  );
}

/* ─── エメラルドブロック質感 SVG (カード地紋) ─────────────────────── */
function EmeraldBlockBg({ idSuffix = "a" }) {
  const pid = "emeblock-" + idSuffix;
  return (
    <svg width="100%" height="100%" preserveAspectRatio="none"
      style={{ position: "absolute", inset: 0 }}>
      <defs>
        <pattern id={pid} width="58" height="58" patternUnits="userSpaceOnUse" patternTransform="rotate(0)">
          <rect width="58" height="58" fill="#147a44" />
          {/* one cut-emerald cell */}
          <polygon points="20,4 38,4 54,20 54,38 38,54 20,54 4,38 4,20" fill="#1f9e57" />
          <polygon points="20,4 38,4 46,14 12,14" fill="#3bd47e" opacity="0.9" />
          <polygon points="20,54 38,54 46,44 12,44" fill="#0f7d41" />
          <polygon points="4,20 12,14 12,44 4,38" fill="#19a559" />
          <polygon points="54,20 46,14 46,44 54,38" fill="#127a42" />
          <polygon points="12,14 46,14 46,44 12,44" fill="#27b566" />
          <polygon points="12,14 46,14 29,29" fill="#4fe28d" opacity="0.85" />
          <polygon points="12,14 29,29 12,44" fill="#34c878" opacity="0.7" />
          <polygon points="16,17 24,17 20,21" fill="#cffce2" opacity="0.7" />
        </pattern>
      </defs>
      <rect width="100%" height="100%" fill={`url(#${pid})`} />
    </svg>
  );
}

/* ─── MoyMoy カード (PayPay × クレカ) ─────────────────────────────── */
function MoyMoyCard({ number = "5089 2271 0043 6618", holder = "STEVE", expiry = "07/29",
  balance, compact = false }) {
  return (
    <div style={{ position: "relative", width: "100%", aspectRatio: compact ? "1.9 / 1" : "1.62 / 1",
      border: "1.5px solid #000", boxShadow: "6px 6px 0 #0B5A33", overflow: "hidden",
      color: "#F2FFF7", isolation: "isolate" }}>
      <EmeraldBlockBg idSuffix="card" />
      {/* multiply facet sheen */}
      <svg viewBox="0 0 100 62" preserveAspectRatio="none"
        style={{ position: "absolute", inset: 0, width: "100%", height: "100%", mixBlendMode: "multiply", opacity: 0.5 }}>
        <polygon points="0,0 64,0 0,40" fill="#0B7A41" opacity="0.5" />
        <polygon points="100,62 40,62 100,24" fill="#0B5A33" opacity="0.6" />
      </svg>
      {/* top gloss */}
      <div style={{ position: "absolute", inset: 0,
        background: "linear-gradient(135deg, rgba(255,255,255,0.34) 0%, transparent 42%, rgba(0,0,0,0.28) 100%)" }} />
      {/* bottom legibility wash */}
      <div style={{ position: "absolute", left: 0, right: 0, bottom: 0, height: "62%",
        background: "linear-gradient(to top, rgba(6,60,33,0.78), transparent)" }} />
      <div className="paper-noise" style={{ opacity: 0.12, zIndex: 1 }} />

      {/* content */}
      <div style={{ position: "relative", zIndex: 2, height: "100%", padding: "16px 18px",
        display: "flex", flexDirection: "column" }}>
        {/* header row */}
        <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <EmeGem size={22} />
            <span style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800,
              fontSize: 19, letterSpacing: "-0.02em", textShadow: "0 1px 2px rgba(0,0,0,0.35)" }}>MoyMoy</span>
          </div>
          <span style={{ fontFamily: "var(--font-mono)", fontSize: 9, fontWeight: 700,
            letterSpacing: "0.22em", opacity: 0.85 }}>PREPAID</span>
        </div>

        {/* chip */}
        <div style={{ marginTop: compact ? 8 : 14, width: 40, height: 30, position: "relative",
          border: "1px solid rgba(0,0,0,0.4)", background: "linear-gradient(135deg,#EBD27A,#B8923A)",
          boxShadow: "inset 0 1px 2px rgba(255,255,255,0.6)" }}>
          <div style={{ position: "absolute", inset: 4, border: "0.5px solid rgba(0,0,0,0.35)" }} />
          <div style={{ position: "absolute", top: "50%", left: 0, right: 0, height: 1, background: "rgba(0,0,0,0.35)" }} />
        </div>

        <div style={{ flex: 1 }} />

        {/* balance (fused: card shows live balance) */}
        {balance != null && (
          <div style={{ marginBottom: compact ? 6 : 10 }}>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, fontWeight: 700,
              letterSpacing: "0.2em", opacity: 0.8 }}>BALANCE · 残高</div>
            <Eme amount={balance} size={compact ? 26 : 30} color="#F2FFF7" style={{
              textShadow: "0 1px 2px rgba(0,0,0,0.4)" }} />
          </div>
        )}

        {/* embossed number */}
        <div style={{ fontFamily: "var(--font-mono)", fontWeight: 600,
          fontSize: compact ? 16 : 19, letterSpacing: "0.14em",
          textShadow: "0 1px 0 rgba(255,255,255,0.35), 0 -1px 0 rgba(0,0,0,0.45)" }}>
          {number}
        </div>

        {/* footer: holder / expiry */}
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-end", marginTop: 8 }}>
          <div>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 8, letterSpacing: "0.16em", opacity: 0.7 }}>CARD HOLDER</div>
            <div style={{ fontFamily: "var(--font-mono)", fontWeight: 600, fontSize: 12, letterSpacing: "0.1em",
              textShadow: "0 1px 0 rgba(0,0,0,0.4)" }}>{holder} <span style={{ opacity: 0.7 }}>◈</span></div>
          </div>
          <div style={{ textAlign: "right" }}>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 8, letterSpacing: "0.16em", opacity: 0.7 }}>VALID THRU</div>
            <div style={{ fontFamily: "var(--font-mono)", fontWeight: 600, fontSize: 12, letterSpacing: "0.1em" }}>{expiry}</div>
          </div>
        </div>
      </div>
    </div>
  );
}

/* ─── モイモイ♪ チャイム (Web Audio) ─────────────────────────────── */
function playMoyChime() {
  try {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    const ctx = new Ctx();
    const t0 = ctx.currentTime;
    const master = ctx.createGain();
    master.gain.value = 0.5;
    master.connect(ctx.destination);
    // "モイ・モイ" — two bright rising motifs (E5→B5 ×2)
    const motif = [
      [659.25, 0.00], [987.77, 0.10],
      [659.25, 0.34], [987.77, 0.44],
    ];
    motif.forEach(([f, t]) => {
      const o = ctx.createOscillator();
      const g = ctx.createGain();
      o.type = "triangle";
      o.frequency.setValueAtTime(f, t0 + t);
      o.connect(g); g.connect(master);
      g.gain.setValueAtTime(0.0001, t0 + t);
      g.gain.exponentialRampToValueAtTime(0.32, t0 + t + 0.015);
      g.gain.exponentialRampToValueAtTime(0.0001, t0 + t + 0.30);
      o.start(t0 + t); o.stop(t0 + t + 0.34);
    });
    // sparkle tail (gem shimmer)
    [1318.5, 1567.98, 1975.5].forEach((f, i) => {
      const o = ctx.createOscillator();
      const g = ctx.createGain();
      const t = 0.58 + i * 0.05;
      o.type = "sine"; o.frequency.value = f;
      o.connect(g); g.connect(master);
      g.gain.setValueAtTime(0.0001, t0 + t);
      g.gain.exponentialRampToValueAtTime(0.14, t0 + t + 0.01);
      g.gain.exponentialRampToValueAtTime(0.0001, t0 + t + 0.5);
      o.start(t0 + t); o.stop(t0 + t + 0.55);
    });
    setTimeout(() => ctx.close(), 1600);
  } catch (e) { /* no audio */ }
}

/* ─── 決済完了オーバーレイ (PayPay音的演出) ───────────────────────── */
function CompleteOverlay({ kind, target, amount, sound, onClose }) {
  mmEffect(() => {
    if (sound) playMoyChime();
    const t = setTimeout(onClose, 2600);
    return () => clearTimeout(t);
  }, []);
  const verb = kind === "send" ? "送金しました" : kind === "charge" ? "チャージしました" : "支払いました";
  return (
    <div onClick={onClose} style={{ position: "absolute", inset: 0, zIndex: 200,
      display: "flex", flexDirection: "column", alignItems: "center", justifyContent: "center",
      background: "linear-gradient(160deg, #16A35A 0%, #0B7A41 100%)", color: "#fff",
      overflow: "hidden", cursor: "pointer" }}>
      <div className="paper-noise" style={{ opacity: 0.14 }} />
      {/* gem burst rays */}
      <svg viewBox="0 0 400 400" style={{ position: "absolute", width: 620, height: 620,
        opacity: 0.5, animation: "moy-spin 9s linear infinite" }}>
        {Array.from({ length: 18 }).map((_, i) => {
          const a = (i / 18) * Math.PI * 2;
          return <polygon key={i} fill="rgba(255,255,255,0.10)"
            points={`200,200 ${200 + Math.cos(a) * 280},${200 + Math.sin(a) * 280} ${200 + Math.cos(a + 0.18) * 280},${200 + Math.sin(a + 0.18) * 280}`} />;
        })}
      </svg>

      <div style={{ position: "relative", textAlign: "center", animation: "moy-pop 520ms cubic-bezier(.2,1.3,.4,1) both" }}>
        <div style={{ position: "relative", width: 132, height: 132, margin: "0 auto 22px" }}>
          {/* gem */}
          <div style={{ position: "absolute", inset: 0, animation: "moy-gem 600ms cubic-bezier(.2,1.4,.4,1) both" }}>
            <EmeGem size={132} id="big" />
          </div>
          {/* checkmark */}
          <svg viewBox="0 0 60 60" style={{ position: "absolute", inset: 0, width: "100%", height: "100%" }}>
            <path d="M17 31 L26 40 L44 21" fill="none" stroke="#fff" strokeWidth="6"
              strokeLinecap="square" strokeLinejoin="miter"
              style={{ filter: "drop-shadow(0 1px 2px rgba(0,0,0,0.4))",
                strokeDasharray: 70, strokeDashoffset: 70, animation: "moy-check 420ms 360ms ease-out forwards" }} />
          </svg>
        </div>
        <div style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 46,
          letterSpacing: "-0.01em", textShadow: "0 2px 6px rgba(0,0,0,0.3)" }}>モイモイ！</div>
        <div style={{ fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15, opacity: 0.95, marginTop: 2 }}>{verb}</div>
        <div style={{ marginTop: 18 }}>
          <Eme amount={amount} size={44} color="#fff" sign={kind !== "charge"} style={{ textShadow: "0 2px 6px rgba(0,0,0,0.3)" }} />
        </div>
        {target && <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, opacity: 0.9, marginTop: 8 }}>{target} へ</div>}
      </div>

      <div style={{ position: "absolute", bottom: 40, fontFamily: "var(--font-mono)", fontSize: 11,
        letterSpacing: "0.16em", opacity: 0.8 }}>タップで閉じる</div>
    </div>
  );
}

/* ─── ボトムナビ ─────────────────────────────────────────────────── */
function MoyBottomNav({ tab, onTab }) {
  const items = [
    { id: "home", label: "ホーム", glyph: "⌂" },
    { id: "send", label: "送る", glyph: "⇄" },
    { id: "pay", label: "支払う", center: true },
    { id: "charge", label: "チャージ", glyph: "＋" },
    { id: "history", label: "履歴", glyph: "≡" },
  ];
  return (
    <div style={{ position: "absolute", left: 0, right: 0, bottom: 0, zIndex: 60,
      background: "var(--bg-white)", borderTop: "1.5px solid var(--ink)",
      paddingBottom: 22, paddingTop: 8, display: "grid", gridTemplateColumns: "1fr 1fr 1fr 1fr 1fr",
      alignItems: "end" }}>
      {items.map(it => {
        const active = tab === it.id;
        if (it.center) {
          return (
            <button key={it.id} onClick={() => onTab(it.id)} style={{
              justifySelf: "center", border: "1.5px solid #000", background: active ? "var(--moy-deep)" : "var(--moy)",
              color: "#fff", width: 60, height: 60, marginTop: -30, marginBottom: 2,
              boxShadow: "3px 3px 0 #0B5A33", display: "flex", flexDirection: "column",
              alignItems: "center", justifyContent: "center", gap: 2, cursor: "pointer",
              clipPath: "polygon(20% 0,80% 0,100% 30%,100% 70%,80% 100%,20% 100%,0 70%,0 30%)" }}>
              <EmeGem size={22} />
              <span style={{ fontFamily: "var(--font-jp)", fontSize: 10, fontWeight: 700 }}>{it.label}</span>
            </button>
          );
        }
        return (
          <button key={it.id} onClick={() => onTab(it.id)} style={{
            background: "transparent", border: "none", cursor: "pointer",
            display: "flex", flexDirection: "column", alignItems: "center", gap: 3, padding: "2px 0",
            color: active ? "var(--moy-deep)" : "var(--ink-soft)" }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 20, fontWeight: 700, lineHeight: 1 }}>{it.glyph}</span>
            <span style={{ fontFamily: "var(--font-jp)", fontSize: 10, fontWeight: active ? 700 : 500 }}>{it.label}</span>
          </button>
        );
      })}
    </div>
  );
}

/* ─── ヘッダ ─────────────────────────────────────────────────────── */
function MoyHeader({ onClose, account, onMenu }) {
  const initial = account
    ? (Array.from(account.display_name || account.handle || "?")[0] || "?")
    : null;
  return (
    <div style={{ flexShrink: 0, paddingTop: 56, paddingLeft: 18, paddingRight: 18, paddingBottom: 12,
      display: "flex", alignItems: "center", justifyContent: "space-between",
      background: "var(--moy)", color: "#fff", borderBottom: "1.5px solid #000", position: "relative",
      overflow: "hidden" }}>
      <div className="paper-noise" style={{ opacity: 0.1 }} />
      <button onClick={onClose} aria-label="close" style={{ position: "relative", zIndex: 1,
        background: "transparent", border: "none", cursor: "pointer", color: "#fff",
        display: "flex", alignItems: "center", gap: 7,
        fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700, letterSpacing: "0.16em" }}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
          <path d="M15 18l-6-6 6-6" />
        </svg>
        HOME
      </button>
      <div style={{ position: "relative", zIndex: 1, display: "flex", alignItems: "center", gap: 7 }}>
        <EmeGem size={20} />
        <span style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 18,
          letterSpacing: "-0.02em" }}>MoyMoy</span>
      </div>
      {onMenu && account ? (
        <button onClick={onMenu} aria-label="account" style={{ position: "relative", zIndex: 1,
          width: 32, height: 32, flexShrink: 0, border: "1.5px solid #fff", background: "rgba(255,255,255,0.18)",
          color: "#fff", cursor: "pointer", display: "grid", placeItems: "center",
          fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 15, padding: 0 }}>
          {initial}
        </button>
      ) : (
        <div style={{ width: 32 }} />
      )}
    </div>
  );
}

/* ─── ホーム画面 ─────────────────────────────────────────────────── */
function MoyHome({ balance, txns, profile, onTab }) {
  return (
    <div style={{ padding: "18px 18px 120px" }}>
      {/* balance hero */}
      <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between" }}>
        <div>
          <div className="eyebrow" style={{ color: "var(--moy-deep)" }}>利用可能残高</div>
          <Eme amount={balance} size={46} color="var(--ink)" />
          <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)",
            letterSpacing: "0.04em", marginTop: 4 }}>≈ {formatEme(toBlocks(balance))} エメラルドブロック</div>
        </div>
      </div>

      {/* card */}
      <div style={{ marginTop: 16 }}>
        <MoyMoyCard balance={balance} holder={profile.holder} number={profile.number} expiry={profile.expiry} />
      </div>

      {/* quick actions */}
      <div style={{ marginTop: 18, display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 10 }}>
        {[
          { id: "pay", label: "支払う", glyph: "◈" },
          { id: "send", label: "送る", glyph: "⇄" },
          { id: "charge", label: "チャージ", glyph: "＋" },
        ].map(a => (
          <button key={a.id} onClick={() => onTab(a.id)} style={{
            border: "1.5px solid var(--ink)", background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)",
            padding: "14px 6px", display: "flex", flexDirection: "column", alignItems: "center", gap: 7,
            cursor: "pointer", transition: "transform 120ms" }}>
            <span style={{ width: 34, height: 34, display: "grid", placeItems: "center",
              background: "var(--moy-mint)", border: "1.5px solid var(--moy-deep)",
              fontFamily: "var(--font-mono)", fontSize: 17, fontWeight: 700, color: "var(--moy-deep)" }}>{a.glyph}</span>
            <span style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700 }}>{a.label}</span>
          </button>
        ))}
      </div>

      {/* recent */}
      <div style={{ marginTop: 24, display: "flex", alignItems: "baseline", justifyContent: "space-between" }}>
        <div className="h-section">最近の取引</div>
        <button onClick={() => onTab("history")} style={{ background: "transparent", border: "none",
          cursor: "pointer", fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700,
          letterSpacing: "0.08em", color: "var(--moy-deep)" }}>すべて →</button>
      </div>
      <div style={{ marginTop: 10, border: "1.5px solid var(--ink)" }}>
        {txns.slice(0, 4).map((t, i) => (
          <TxnRow key={t.id} t={t} last={i === Math.min(3, txns.length - 1)} />
        ))}
        {txns.length === 0 && (
          <div style={{ padding: 20, textAlign: "center", fontFamily: "var(--font-jp)",
            fontSize: 13, color: "var(--ink-soft)" }}>まだ取引はありません</div>
        )}
      </div>
    </div>
  );
}

/* ─── 取引行 (履歴で共用) ───────────────────────────────────────── */
function TxnRow({ t, last }) {
  const incoming = t.amount > 0;
  const meta = {
    pay:     { glyph: "◈", pal: "emerald", tag: "支払い" },
    send:    { glyph: "⇄", pal: "turquoise", tag: "送金" },
    receive: { glyph: "↓", pal: "green", tag: "受取" },
    charge:  { glyph: "＋", pal: "meadow", tag: "チャージ" },
  }[t.kind] || { glyph: "◈", pal: "emerald", tag: "" };
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "12px 14px",
      borderBottom: last ? "none" : "1px solid rgba(0,0,0,0.08)", background: "var(--bg-white)" }}>
      <div style={{ width: 38, height: 38, flexShrink: 0 }}>
        <CrystalIcon palette={meta.pal} glyph={meta.glyph} size={38} />
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 600, color: "var(--ink)",
          whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{t.label}</div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--ink-soft)",
          letterSpacing: "0.05em", marginTop: 1 }}>{meta.tag} · {t.time}</div>
      </div>
      <Eme amount={t.amount} size={16} sign gem={false} suffix={false}
        color={incoming ? "var(--moy-deep)" : "var(--ink)"} weight={700} />
    </div>
  );
}

Object.assign(window, {
  formatEme, toBlocks, EmeGem, Eme, EmeraldBlockBg, MoyMoyCard,
  playMoyChime, CompleteOverlay, MoyBottomNav, MoyHeader, MoyHome, TxnRow,
});
