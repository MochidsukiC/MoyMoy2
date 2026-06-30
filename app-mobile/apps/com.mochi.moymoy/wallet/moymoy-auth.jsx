/* global React, MoyMoy, MoyMoyApp, EmeGem, CrystalIcon */
/* =====================================================================
   MoyMoy — auth shell & multi-account (v2)

   MoyMoyRoot wraps the wallet (MoyMoyApp) with an authentication gate:
   independent MoyMoy accounts (handle + PIN). One phone can hold MANY accounts
   (a client-side session list persisted via MoyMoy.store); the user switches
   between them. The active account's session token is fed to the SDK
   (MoyMoy.setSession) so every wallet request is backend-verified.

   Screens: onboarding / register (口座開設) / login / PIN entry / account
   switcher / settings (linked MC characters + logout).
   ===================================================================== */

const { useState: maState, useEffect: maEffect } = React;

const STORE_KEY = "moymoy.accounts.v1";

/* ─── persistence (account/session list) ─────────────────────────────── */
async function loadAccounts() {
  const raw = await MoyMoy.store.get(STORE_KEY);
  if (!raw) return { accounts: [], activeId: null };
  try {
    const o = JSON.parse(raw);
    return {
      accounts: Array.isArray(o.accounts) ? o.accounts : [],
      activeId: o.activeId || null,
    };
  } catch (e) {
    return { accounts: [], activeId: null };
  }
}
async function saveAccounts(accounts, activeId) {
  await MoyMoy.store.set(STORE_KEY, JSON.stringify({ accounts, activeId }));
}

/* ─── error labels ───────────────────────────────────────────────────── */
const REG_ERR = {
  handle_taken: "その ID はすでに使われています",
  bad_handle: "ID は半角英数字と _ の3〜20文字です",
  bad_pin: "PIN は4〜6桁の数字です",
  bad_display_name: "名前を入力してください",
};
const LOGIN_ERR = {
  invalid_credentials: "ID または PIN が違います",
  locked: "試行回数が多すぎます。しばらく待ってから再試行してください",
};

/* ─── small UI atoms ─────────────────────────────────────────────────── */
function PinDots({ len, max = 6 }) {
  return (
    <div style={{ display: "flex", gap: 12, justifyContent: "center" }}>
      {Array.from({ length: max }).map((_, i) => (
        <span key={i} style={{ width: 14, height: 14, border: "1.5px solid var(--ink)",
          background: i < len ? "var(--moy)" : "transparent" }} />
      ))}
    </div>
  );
}

function PinKeypad({ onPress }) {
  const keys = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "", "0", "⌫"];
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 1,
      background: "var(--ink)", border: "1.5px solid var(--ink)" }}>
      {keys.map((k, i) =>
        k === "" ? (
          <div key={i} style={{ background: "var(--bg-white)", height: 58 }} />
        ) : (
          <button key={i} onClick={() => onPress(k)} style={{ height: 58, background: "var(--bg-white)",
            border: "none", cursor: "pointer", fontFamily: "var(--font-mono)", fontWeight: 700,
            fontSize: k === "⌫" ? 20 : 24, color: "var(--ink)" }}>{k}</button>
        )
      )}
    </div>
  );
}

const fieldStyle = {
  width: "100%", padding: "13px 14px", border: "1.5px solid var(--ink)", background: "var(--bg-white)",
  boxShadow: "3px 3px 0 var(--ink)", fontFamily: "var(--font-jp)", fontSize: 15, color: "var(--ink)",
  outline: "none", boxSizing: "border-box",
};
const ctaStyle = (disabled) => ({
  width: "100%", padding: "16px", border: "1.5px solid #000",
  background: disabled ? "#bdbdbd" : "var(--moy)", color: "#fff",
  boxShadow: disabled ? "none" : "4px 4px 0 #0B5A33", cursor: disabled ? "default" : "pointer",
  fontFamily: "var(--font-jp)", fontWeight: 800, fontSize: 17, letterSpacing: "0.04em",
  display: "flex", alignItems: "center", justifyContent: "center", gap: 8,
});

function AuthScene({ title, sub, onBack, children }) {
  return (
    <div className="screen fade-in" style={{ position: "absolute", inset: 0, zIndex: 30,
      background: "var(--bg-white)", display: "flex", flexDirection: "column" }}>
      <div style={{ flexShrink: 0, padding: "56px 18px 16px", background: "var(--moy)", color: "#fff",
        borderBottom: "1.5px solid #000", position: "relative", overflow: "hidden" }}>
        <div className="paper-noise" style={{ opacity: 0.1 }} />
        <div style={{ position: "relative", zIndex: 1 }}>
          {onBack && (
            <button onClick={onBack} style={{ background: "transparent", border: "none", cursor: "pointer",
              color: "#fff", fontFamily: "var(--font-mono)", fontSize: 11, fontWeight: 700,
              letterSpacing: "0.16em", display: "flex", alignItems: "center", gap: 6, marginBottom: 10 }}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5"><path d="M15 18l-6-6 6-6" /></svg>戻る
            </button>
          )}
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <EmeGem size={26} />
            <div>
              <div style={{ fontFamily: "'Archivo', var(--font-sans)", fontWeight: 800, fontSize: 22,
                letterSpacing: "-0.02em" }}>{title}</div>
              {sub && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, opacity: 0.9 }}>{sub}</div>}
            </div>
          </div>
        </div>
      </div>
      <div style={{ flex: 1, overflow: "auto" }}>{children}</div>
    </div>
  );
}

/* ─── onboarding ─────────────────────────────────────────────────────── */
function AuthWelcome({ canCancel, onRegister, onLogin, onCancel }) {
  return (
    <AuthScene title="MoyMoy" sub="ゲーム内エメラルド決済ウォレット" onBack={canCancel ? onCancel : null}>
      <div style={{ padding: "28px 22px", display: "flex", flexDirection: "column", gap: 16 }}>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, color: "var(--ink-soft)", lineHeight: 1.8 }}>
          MoyMoy の口座を{canCancel ? "追加" : "作成"}します。口座は PayPay のように MoyMoy 独自のアカウントで、
          ID と PIN で保護されます。
        </div>
        <button onClick={onRegister} style={ctaStyle(false)}>
          <EmeGem size={20} /> 口座を開設する
        </button>
        <button onClick={onLogin} style={{ width: "100%", padding: "15px", border: "1.5px solid var(--ink)",
          background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)", cursor: "pointer",
          fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 15, color: "var(--ink)" }}>
          すでに口座をお持ちの方（ログイン）
        </button>
      </div>
    </AuthScene>
  );
}

/* ─── register (口座開設) ────────────────────────────────────────────── */
function AuthRegister({ onDone, onBack }) {
  const [step, setStep] = maState("form"); // form | pin | confirm
  const [name, setName] = maState("");
  const [handle, setHandle] = maState("");
  const [pin, setPin] = maState("");
  const [pin2, setPin2] = maState("");
  const [busy, setBusy] = maState(false);
  const [err, setErr] = maState("");

  const handleOk = /^[A-Za-z0-9_]{3,20}$/.test(handle.trim());
  const nameOk = name.trim().length >= 1 && name.trim().length <= 24;

  function pressPin(k) {
    setErr("");
    if (step === "pin") setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
    else setPin2((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  }

  async function submit() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.register({ handle: handle.trim(), display_name: name.trim(), pin });
      if (r.ok && r.session) {
        onDone(r.account, r.session);
        return;
      }
      setErr(REG_ERR[r.error] || "登録に失敗しました");
      setStep("form");
      setPin("");
      setPin2("");
    } catch (e) {
      setErr("通信に失敗しました");
      setStep("form");
      setPin("");
      setPin2("");
    }
    setBusy(false);
  }

  if (step === "pin" || step === "confirm") {
    const cur = step === "pin" ? pin : pin2;
    const title = step === "pin" ? "PIN を設定" : "PIN を再入力";
    const canNext = cur.length >= 4;
    const onNext = () => {
      if (step === "pin") {
        setStep("confirm");
      } else if (pin === pin2) {
        submit();
      } else {
        setErr("PIN が一致しません");
        setStep("pin");
        setPin("");
        setPin2("");
      }
    };
    return (
      <AuthScene title="口座を開設" sub={"@" + (handle || "id")} onBack={() => { setStep("form"); setPin(""); setPin2(""); setErr(""); }}>
        <div style={{ padding: "30px 22px", display: "flex", flexDirection: "column", alignItems: "center", gap: 22 }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700 }}>{title}</div>
          <PinDots len={cur.length} />
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)" }}>4〜6桁の数字</div>
          {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
          <div style={{ width: "100%", maxWidth: 320 }}>
            <PinKeypad onPress={pressPin} />
            <button disabled={!canNext || busy} onClick={onNext} style={{ ...ctaStyle(!canNext || busy), marginTop: 14 }}>
              {busy ? "処理中…" : step === "pin" ? "次へ" : "口座を開設"}
            </button>
          </div>
        </div>
      </AuthScene>
    );
  }

  return (
    <AuthScene title="口座を開設" sub="名前と ID を決めてください" onBack={onBack}>
      <div style={{ padding: "24px 22px", display: "flex", flexDirection: "column", gap: 18 }}>
        <div>
          <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>表示名</div>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="アリス" maxLength={24} style={fieldStyle} />
        </div>
        <div>
          <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>MoyMoy ID（送金の宛先）</div>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 18, fontWeight: 700, color: "var(--ink-soft)" }}>@</span>
            <input value={handle} onChange={(e) => setHandle(e.target.value)} placeholder="alice" maxLength={20} style={fieldStyle} />
          </div>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", marginTop: 6 }}>
            半角英数字と _ の3〜20文字
          </div>
        </div>
        {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
        <button disabled={!handleOk || !nameOk} onClick={() => { setErr(""); setStep("pin"); }} style={ctaStyle(!handleOk || !nameOk)}>
          PIN の設定へ
        </button>
      </div>
    </AuthScene>
  );
}

/* ─── login ──────────────────────────────────────────────────────────── */
function AuthLogin({ onDone, onBack }) {
  const [step, setStep] = maState("form"); // form | pin
  const [handle, setHandle] = maState("");
  const [pin, setPin] = maState("");
  const [busy, setBusy] = maState(false);
  const [err, setErr] = maState("");

  const handleOk = handle.trim().length >= 3;

  function pressPin(k) {
    setErr("");
    setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  }

  async function submit() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.login({ handle: handle.trim(), pin });
      if (r.ok && r.session) {
        onDone(r.account, r.session);
        return;
      }
      setErr(LOGIN_ERR[r.error] || "ログインに失敗しました");
      setPin("");
    } catch (e) {
      setErr("通信に失敗しました");
      setPin("");
    }
    setBusy(false);
  }

  if (step === "pin") {
    return (
      <AuthScene title="ログイン" sub={"@" + handle.trim()} onBack={() => { setStep("form"); setPin(""); setErr(""); }}>
        <div style={{ padding: "30px 22px", display: "flex", flexDirection: "column", alignItems: "center", gap: 22 }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700 }}>PIN を入力</div>
          <PinDots len={pin.length} />
          {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--carle-red)", textAlign: "center" }}>{err}</div>}
          <div style={{ width: "100%", maxWidth: 320 }}>
            <PinKeypad onPress={pressPin} />
            <button disabled={pin.length < 4 || busy} onClick={submit} style={{ ...ctaStyle(pin.length < 4 || busy), marginTop: 14 }}>
              {busy ? "確認中…" : "ログイン"}
            </button>
          </div>
        </div>
      </AuthScene>
    );
  }

  return (
    <AuthScene title="ログイン" sub="MoyMoy ID と PIN" onBack={onBack}>
      <div style={{ padding: "24px 22px", display: "flex", flexDirection: "column", gap: 18 }}>
        <div>
          <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>MoyMoy ID</div>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 18, fontWeight: 700, color: "var(--ink-soft)" }}>@</span>
            <input value={handle} onChange={(e) => setHandle(e.target.value.replace(/^@/, ""))} placeholder="alice" maxLength={20} style={fieldStyle} />
          </div>
        </div>
        {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
        <button disabled={!handleOk} onClick={() => { setErr(""); setStep("pin"); }} style={ctaStyle(!handleOk)}>
          次へ
        </button>
      </div>
    </AuthScene>
  );
}

/* ─── account switcher (bottom sheet) ────────────────────────────────── */
function AccountMenu({ open, accounts, activeId, onSwitch, onAdd, onLogout, onSettings, onClose }) {
  if (!open) return null;
  return (
    <div style={{ position: "absolute", inset: 0, zIndex: 130, display: "flex", flexDirection: "column",
      justifyContent: "flex-end", background: "rgba(10,30,18,0.45)" }} onClick={onClose}>
      <div className="slide-up" onClick={(e) => e.stopPropagation()} style={{ background: "var(--bg-white)",
        borderTop: "1.5px solid var(--ink)", padding: "18px 0 24px", maxHeight: "80%", overflow: "auto" }}>
        <div style={{ width: 44, height: 4, background: "var(--ink)", margin: "0 auto 14px", opacity: 0.4 }} />
        <div className="eyebrow" style={{ color: "var(--moy-deep)", textAlign: "center", marginBottom: 12 }}>アカウント</div>
        <div style={{ borderTop: "1.5px solid var(--ink)", borderBottom: "1.5px solid var(--ink)" }}>
          {accounts.map((a) => {
            const active = a.account_id === activeId;
            const label = a.display_name || ("@" + a.handle);
            const glyph = Array.from(label)[0] || "?";
            return (
              <div key={a.account_id} style={{ display: "flex", alignItems: "center", gap: 12, padding: "12px 16px",
                borderBottom: "1px solid rgba(0,0,0,0.08)", background: active ? "var(--moy-mint)" : "var(--bg-white)" }}>
                <button onClick={() => onSwitch(a.account_id)} style={{ flex: 1, display: "flex", alignItems: "center",
                  gap: 12, background: "transparent", border: "none", cursor: "pointer", textAlign: "left", minWidth: 0 }}>
                  <div style={{ width: 40, height: 40, flexShrink: 0 }}>
                    <CrystalIcon palette={active ? "emerald" : "turquoise"} glyph={glyph} size={40} />
                  </div>
                  <div style={{ minWidth: 0 }}>
                    <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700, color: "var(--ink)" }}>{label}</div>
                    <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--ink-soft)" }}>@{a.handle}{active ? " · 利用中" : ""}</div>
                  </div>
                </button>
                <button onClick={() => onLogout(a.account_id)} aria-label="logout" style={{ background: "transparent",
                  border: "none", cursor: "pointer", color: "var(--carle-red)", fontFamily: "var(--font-mono)",
                  fontSize: 11, fontWeight: 700, letterSpacing: "0.08em" }}>ログアウト</button>
              </div>
            );
          })}
        </div>
        <div style={{ padding: "14px 16px 0", display: "flex", flexDirection: "column", gap: 10 }}>
          <button onClick={onAdd} style={{ width: "100%", padding: "13px", border: "1.5px solid var(--ink)",
            background: "var(--bg-white)", boxShadow: "3px 3px 0 var(--ink)", cursor: "pointer",
            fontFamily: "var(--font-jp)", fontWeight: 700, fontSize: 14, color: "var(--ink)" }}>＋ アカウントを追加</button>
          <button onClick={onSettings} style={{ width: "100%", padding: "13px", border: "none", background: "transparent",
            cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 600, fontSize: 13, color: "var(--ink-soft)" }}>設定 · 連携キャラクター</button>
        </div>
      </div>
    </div>
  );
}

/* ─── settings (linked MC characters + logout) ───────────────────────── */
function SettingsSheet({ open, account, onLogout, onClose }) {
  const [links, setLinks] = maState([]);
  const [loaded, setLoaded] = maState(false);
  maEffect(() => {
    if (!open) return;
    setLoaded(false);
    let alive = true;
    MoyMoy.me()
      .then((r) => { if (alive && r.ok) setLinks(r.linked_mc || []); })
      .catch(() => {})
      .finally(() => { if (alive) setLoaded(true); });
    return () => { alive = false; };
  }, [open]);
  if (!open) return null;
  return (
    <div style={{ position: "absolute", inset: 0, zIndex: 140, display: "flex", flexDirection: "column",
      justifyContent: "flex-end", background: "rgba(10,30,18,0.45)" }} onClick={onClose}>
      <div className="slide-up" onClick={(e) => e.stopPropagation()} style={{ background: "var(--bg-white)",
        borderTop: "1.5px solid var(--ink)", padding: "18px 18px 26px", maxHeight: "80%", overflow: "auto" }}>
        <div style={{ width: 44, height: 4, background: "var(--ink)", margin: "0 auto 14px", opacity: 0.4 }} />
        <div className="eyebrow" style={{ color: "var(--moy-deep)", textAlign: "center" }}>設定</div>
        {account && (
          <div style={{ textAlign: "center", marginTop: 12 }}>
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 16, fontWeight: 700 }}>{account.display_name || ("@" + account.handle)}</div>
            <div style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: "var(--ink-soft)" }}>@{account.handle}</div>
          </div>
        )}
        <div className="h-section" style={{ marginTop: 20, marginBottom: 8 }}>連携キャラクター</div>
        <div style={{ border: "1.5px solid var(--ink)" }}>
          {links.map((l) => (
            <div key={l.mc_uuid} style={{ display: "flex", alignItems: "center", gap: 12, padding: "12px 14px",
              borderBottom: "1px solid rgba(0,0,0,0.08)" }}>
              <div style={{ width: 34, height: 34 }}><CrystalIcon palette="meadow" glyph={(l.mcid && Array.from(l.mcid)[0]) || "◈"} size={34} /></div>
              <div style={{ minWidth: 0 }}>
                <div style={{ fontFamily: "var(--font-jp)", fontSize: 14, fontWeight: 600 }}>{l.mcid || "（名前不明）"}</div>
                <div style={{ fontFamily: "var(--font-mono)", fontSize: 9, color: "var(--ink-soft)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{l.mc_uuid}</div>
              </div>
            </div>
          ))}
          {loaded && links.length === 0 && (
            <div style={{ padding: 18, textAlign: "center", fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)" }}>
              まだ連携キャラクターはありません。<br />チャージするとそのキャラクターが自動で連携されます。
            </div>
          )}
        </div>
        <button onClick={onLogout} style={{ width: "100%", marginTop: 20, padding: "14px", border: "1.5px solid var(--carle-red)",
          background: "rgba(227,38,54,0.06)", cursor: "pointer", fontFamily: "var(--font-jp)", fontWeight: 700,
          fontSize: 15, color: "var(--carle-red)" }}>このアカウントからログアウト</button>
      </div>
    </div>
  );
}

/* ─── root shell ─────────────────────────────────────────────────────── */
function MoyMoyRoot({ onClose }) {
  const [phase, setPhase] = maState("loading"); // loading | auth | app
  const [accounts, setAccounts] = maState([]);
  const [activeId, setActiveId] = maState(null);
  const [authMode, setAuthMode] = maState("welcome"); // welcome | register | login
  const [adding, setAdding] = maState(false);

  // Boot: restore the persisted session list and validate the active one.
  maEffect(() => {
    let alive = true;
    (async () => {
      const { accounts: list, activeId: aid } = await loadAccounts();
      if (!alive) return;
      const active = list.find((a) => a.account_id === aid) || list[0] || null;
      if (active) {
        MoyMoy.setSession(active.session);
        try {
          const me = await MoyMoy.me();
          if (alive && me.ok) {
            setAccounts(list);
            setActiveId(active.account_id);
            setPhase("app");
            return;
          }
        } catch (e) { /* fall through to auth */ }
        // Active session invalid — drop it, keep the rest for manual re-login.
        if (!alive) return;
        const rest = list.filter((a) => a.account_id !== active.account_id);
        setAccounts(rest);
        setActiveId(null);
        MoyMoy.setSession(null);
        await saveAccounts(rest, null);
      }
      if (alive) { setPhase("auth"); setAuthMode("welcome"); }
    })();
    return () => { alive = false; };
  }, []);

  async function onAuthDone(account, session) {
    const entry = { account_id: account.account_id, handle: account.handle, display_name: account.display_name, session };
    const rest = accounts.filter((a) => a.account_id !== entry.account_id);
    const list = [...rest, entry];
    setAccounts(list);
    setActiveId(entry.account_id);
    MoyMoy.setSession(session);
    await saveAccounts(list, entry.account_id);
    setAdding(false);
    setPhase("app");
  }

  async function switchTo(id) {
    const acc = accounts.find((a) => a.account_id === id);
    if (!acc || id === activeId) return;
    MoyMoy.setSession(acc.session);
    try {
      const me = await MoyMoy.me();
      if (me.ok) {
        setActiveId(id);
        await saveAccounts(accounts, id);
        return;
      }
    } catch (e) { /* invalid below */ }
    // Session expired — drop it and send the user to login for that handle.
    const rest = accounts.filter((a) => a.account_id !== id);
    setAccounts(rest);
    if (activeId) { const cur = rest.find((a) => a.account_id === activeId); MoyMoy.setSession(cur ? cur.session : null); }
    await saveAccounts(rest, activeId && rest.find((a) => a.account_id === activeId) ? activeId : null);
    setAdding(true);
    setAuthMode("login");
    setPhase("auth");
  }

  async function logoutAccount(id) {
    const acc = accounts.find((a) => a.account_id === id);
    if (acc) {
      MoyMoy.setSession(acc.session);
      try { await MoyMoy.logout(); } catch (e) { /* ignore */ }
    }
    const rest = accounts.filter((a) => a.account_id !== id);
    let nextActive = activeId;
    if (id === activeId) nextActive = rest.length ? rest[0].account_id : null;
    setAccounts(rest);
    setActiveId(nextActive);
    await saveAccounts(rest, nextActive);
    if (nextActive) {
      const na = rest.find((a) => a.account_id === nextActive);
      MoyMoy.setSession(na.session);
      setPhase("app");
    } else {
      MoyMoy.setSession(null);
      setAdding(false);
      setAuthMode("welcome");
      setPhase("auth");
    }
  }

  function addAccount() {
    setAdding(true);
    setAuthMode("welcome");
    setPhase("auth");
  }
  function cancelAdd() {
    if (accounts.find((a) => a.account_id === activeId)) {
      setAdding(false);
      setPhase("app");
    }
  }

  if (phase === "loading") {
    return (
      <div className="screen" style={{ position: "absolute", inset: 0, display: "grid", placeItems: "center",
        background: "var(--bg-white)" }}>
        <EmeGem size={48} />
      </div>
    );
  }

  if (phase === "auth") {
    if (authMode === "register") return <AuthRegister onDone={onAuthDone} onBack={() => setAuthMode("welcome")} />;
    if (authMode === "login") return <AuthLogin onDone={onAuthDone} onBack={() => setAuthMode("welcome")} />;
    return <AuthWelcome canCancel={adding && !!accounts.find((a) => a.account_id === activeId)}
      onRegister={() => setAuthMode("register")} onLogin={() => setAuthMode("login")} onCancel={cancelAdd} />;
  }

  const active = accounts.find((a) => a.account_id === activeId) || null;
  return (
    <MoyMoyApp
      key={activeId}
      onClose={onClose}
      account={active}
      accounts={accounts}
      onSwitchAccount={switchTo}
      onAddAccount={addAccount}
      onLogoutAccount={logoutAccount}
    />
  );
}

Object.assign(window, {
  MoyMoyRoot, AuthWelcome, AuthRegister, AuthLogin, AccountMenu, SettingsSheet, PinKeypad, PinDots,
});
