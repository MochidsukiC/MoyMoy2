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
    console.error("MoyMoy: stored account list is corrupt JSON; resetting to empty", e);
    return { accounts: [], activeId: null };
  }
}
async function saveAccounts(accounts, activeId) {
  const ok = await MoyMoy.store.set(STORE_KEY, JSON.stringify({ accounts, activeId }));
  if (!ok) console.error("MoyMoy: failed to persist the account list — accounts may not survive an app restart");
  return ok;
}

/* ─── error labels ───────────────────────────────────────────────────── */
const REG_ERR = {
  handle_taken: "その ID はすでに使われています",
  bad_handle: "ID は半角英数字と _ の3〜20文字です",
  bad_pin: "PIN は4〜6桁の数字です",
  bad_display_name: "名前を入力してください",
  bad_email: "MNN メールアドレス（@*.mnn）を入力してください",
  email_taken: "このメールアドレスはすでに使われています",
  too_soon: "コードを送信済みです。少し待ってから再送してください",
  invalid_code: "確認コードが正しくありません",
};
const LOGIN_ERR = {
  invalid_credentials: "ID または PIN が違います",
  locked: "試行回数が多すぎます。しばらく待ってから再試行してください",
  invalid_code: "確認コードが正しくありません",
  recovery_unavailable: "この環境では PIN の再設定は利用できません",
};

/// MNN mail address check for the input field: local@<disc>.mnn with a
/// single-label discriminator (the server validates authoritatively).
function emailLooksOk(s) {
  const t = (s || "").trim();
  return /^[^\s@]+@[^\s@.]+\.mnn$/.test(t);
}

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
          MoyMoy の口座を{canCancel ? "追加" : "作成"}します。口座は MoyMoy 独自のアカウントで、
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

/* ─── emailed-code entry (signup verify / 2FA / recovery) ────────────── */
function CodeEntry({ title, sub, email, hint, cta, onSubmit, onResend, onBack, busy, err }) {
  const [code, setCode] = maState("");
  const press = (k) => setCode((c) => (k === "⌫" ? c.slice(0, -1) : c.length >= 6 ? c : c + k));
  return (
    <AuthScene title={title} sub={sub} onBack={onBack}>
      <div style={{ padding: "24px 22px", display: "flex", flexDirection: "column", alignItems: "center", gap: 18 }}>
        {(email || hint) && (
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", textAlign: "center", lineHeight: 1.7 }}>
            {email ? <>{email} に確認コードを送信しました。<br />メールの6桁コードを入力してください。</> : hint}
          </div>
        )}
        <PinDots len={code.length} />
        {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--carle-red)", textAlign: "center" }}>{err}</div>}
        <div style={{ width: "100%", maxWidth: 320 }}>
          <PinKeypad onPress={press} />
          <button disabled={code.length < 6 || busy} onClick={() => { const c = code; setCode(""); onSubmit(c); }} style={{ ...ctaStyle(code.length < 6 || busy), marginTop: 14 }}>
            {busy ? "確認中…" : cta || "確認する"}
          </button>
          {onResend && (
            <button onClick={onResend} disabled={busy} style={{ width: "100%", marginTop: 10, border: "none", background: "transparent",
              cursor: busy ? "default" : "pointer", fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>
              コードを再送する
            </button>
          )}
        </div>
      </div>
    </AuthScene>
  );
}

/* ─── register (口座開設) ────────────────────────────────────────────── */
function AuthRegister({ emailEnabled, onDone, onBack }) {
  const [step, setStep] = maState("form"); // form | pin | confirm | code
  const [name, setName] = maState("");
  const [handle, setHandle] = maState("");
  const [email, setEmail] = maState("");
  const [pin, setPin] = maState("");
  const [pin2, setPin2] = maState("");
  const [pendingEmail, setPendingEmail] = maState("");
  const [busy, setBusy] = maState(false);
  const [err, setErr] = maState("");

  const handleOk = /^[A-Za-z0-9_]{3,20}$/.test(handle.trim());
  const nameOk = name.trim().length >= 1 && name.trim().length <= 24;
  const emailOk = !emailEnabled || emailLooksOk(email);

  function pressPin(k) {
    setErr("");
    if (step === "pin") setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
    else setPin2((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  }

  function backToForm(msg) {
    setErr(msg || "");
    setStep("form");
    setPin("");
    setPin2("");
  }

  async function submit() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.register({
        handle: handle.trim(),
        display_name: name.trim(),
        pin,
        email: emailEnabled ? email.trim() : undefined,
      });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      if (r.ok && r.pending === "verify_email") { setPendingEmail(r.email || email.trim()); setStep("code"); setBusy(false); return; }
      backToForm(REG_ERR[r.error] || "登録に失敗しました");
    } catch (e) {
      console.warn("MoyMoy: register failed (network/server/parse)", e);
      backToForm("通信に失敗しました");
    }
    setBusy(false);
  }

  async function verifyCode(code) {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.registerVerify({ email: pendingEmail, code });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      setErr(REG_ERR[r.error] || "確認に失敗しました");
    } catch (e) {
      setErr("通信に失敗しました");
    }
    setBusy(false);
  }

  // Dedicated resend handler for the code screen: stays on the code screen and
  // shows inline errors (too_soon etc.) rather than bouncing back to the form.
  async function resend() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.register({
        handle: handle.trim(),
        display_name: name.trim(),
        pin,
        email: emailEnabled ? email.trim() : undefined,
      });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      if (r.ok && r.pending === "verify_email") { setPendingEmail(r.email || email.trim()); }
      else setErr(REG_ERR[r.error] || "再送に失敗しました");
    } catch (e) {
      setErr("通信に失敗しました");
    }
    setBusy(false);
  }

  if (step === "code") {
    return (
      <CodeEntry title="メール確認" sub={"@" + handle.trim()} email={pendingEmail} cta="口座を開設"
        onSubmit={verifyCode} onResend={resend} busy={busy} err={err} onBack={() => backToForm("")} />
    );
  }

  if (step === "pin" || step === "confirm") {
    const cur = step === "pin" ? pin : pin2;
    const title = step === "pin" ? "PIN を設定" : "PIN を再入力";
    const canNext = cur.length >= 4;
    const onNext = () => {
      if (step === "pin") setStep("confirm");
      else if (pin === pin2) submit();
      else { setErr("PIN が一致しません"); setStep("pin"); setPin(""); setPin2(""); }
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
              {busy ? "処理中…" : step === "pin" ? "次へ" : emailEnabled ? "確認コードを送る" : "口座を開設"}
            </button>
          </div>
        </div>
      </AuthScene>
    );
  }

  return (
    <AuthScene title="口座を開設" sub={emailEnabled ? "名前・ID・メールを設定" : "名前と ID を決めてください"} onBack={onBack}>
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
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", marginTop: 6 }}>半角英数字と _ の3〜20文字</div>
        </div>
        {emailEnabled && (
          <div>
            <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>MNN メールアドレス（本人確認・PIN再設定）</div>
            <input value={email} onChange={(e) => setEmail(e.target.value)} placeholder="you@usermail.mnn" type="text" maxLength={254} style={fieldStyle} />
            <div style={{ fontFamily: "var(--font-jp)", fontSize: 11, color: "var(--ink-soft)", marginTop: 6 }}>確認コードを電話のメールアプリに送信します。1アドレス＝1口座。</div>
          </div>
        )}
        {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
        <button disabled={!handleOk || !nameOk || !emailOk} onClick={() => { setErr(""); setStep("pin"); }} style={ctaStyle(!handleOk || !nameOk || !emailOk)}>
          PIN の設定へ
        </button>
      </div>
    </AuthScene>
  );
}

/* ─── login ──────────────────────────────────────────────────────────── */
function AuthLogin({ emailEnabled, onDone, onBack, onForgot }) {
  const [step, setStep] = maState("form"); // form | pin | code
  const [handle, setHandle] = maState("");
  const [pin, setPin] = maState("");
  const [pendingEmail, setPendingEmail] = maState("");
  const [busy, setBusy] = maState(false);
  const [err, setErr] = maState("");

  const handleOk = /^[A-Za-z0-9_]{3,20}$/.test(handle.trim());

  function pressPin(k) {
    setErr("");
    setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  }

  async function submit() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.login({ handle: handle.trim(), pin });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      if (r.ok && r.pending === "2fa") { setPendingEmail(r.email || ""); setStep("code"); setBusy(false); return; }
      setErr(LOGIN_ERR[r.error] || "ログインに失敗しました");
      setPin("");
    } catch (e) {
      console.warn("MoyMoy: login failed (network/server/parse)", e);
      setErr("通信に失敗しました");
      setPin("");
    }
    setBusy(false);
  }

  async function verifyCode(code) {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.loginVerify({ handle: handle.trim(), code });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      setErr(LOGIN_ERR[r.error] || "確認に失敗しました");
    } catch (e) {
      setErr("通信に失敗しました");
    }
    setBusy(false);
  }

  if (step === "code") {
    return (
      <CodeEntry title="2段階認証" sub={"@" + handle.trim()} email={pendingEmail} cta="ログイン"
        onSubmit={verifyCode} onResend={submit} busy={busy} err={err} onBack={() => { setStep("pin"); setErr(""); }} />
    );
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
            {emailEnabled && onForgot && (
              <button onClick={onForgot} disabled={busy} style={{ width: "100%", marginTop: 10, border: "none", background: "transparent",
                cursor: "pointer", fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>
                PIN をお忘れですか？
              </button>
            )}
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
        {emailEnabled && onForgot && (
          <button onClick={onForgot} style={{ width: "100%", border: "none", background: "transparent", cursor: "pointer",
            fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 600, color: "var(--ink-soft)" }}>
            PIN をお忘れですか？
          </button>
        )}
      </div>
    </AuthScene>
  );
}

/* ─── recovery (PIN 再設定) ──────────────────────────────────────────── */
function AuthRecover({ onDone, onBack }) {
  const [step, setStep] = maState("handle"); // handle | code | newpin
  const [handle, setHandle] = maState("");
  const [code, setCode] = maState("");
  const [pin, setPin] = maState("");
  const [busy, setBusy] = maState(false);
  const [err, setErr] = maState("");

  const handleOk = /^[A-Za-z0-9_]{3,20}$/.test(handle.trim());

  async function start() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.recoverStart({ handle: handle.trim() });
      if (r.ok) { setStep("code"); setBusy(false); return; }
      setErr(LOGIN_ERR[r.error] || "送信に失敗しました");
    } catch (e) {
      setErr("通信に失敗しました");
    }
    setBusy(false);
  }

  async function finish() {
    setBusy(true);
    setErr("");
    try {
      const r = await MoyMoy.recoverVerify({ handle: handle.trim(), code, new_pin: pin });
      if (r.ok && r.session) { onDone(r.account, r.session); return; }
      if (r.error === "bad_pin") { setErr("PIN は4〜6桁の数字です"); setPin(""); }
      else { setErr(REG_ERR[r.error] || "確認に失敗しました"); setStep("code"); setCode(""); setPin(""); }
    } catch (e) {
      setErr("通信に失敗しました");
    }
    setBusy(false);
  }

  function pressPin(k) {
    setErr("");
    setPin((p) => (k === "⌫" ? p.slice(0, -1) : p.length >= 6 ? p : p + k));
  }

  if (step === "code") {
    return (
      <CodeEntry title="PIN 再設定" sub={"@" + handle.trim()}
        hint={"@" + handle.trim() + " に登録があれば確認コードを送信しました。コードを入力してください。"} cta="次へ"
        onSubmit={(c) => { setCode(c); setErr(""); setStep("newpin"); }} onResend={start} busy={busy} err={err}
        onBack={() => { setStep("handle"); setErr(""); }} />
    );
  }

  if (step === "newpin") {
    return (
      <AuthScene title="PIN 再設定" sub="新しい PIN" onBack={() => { setStep("code"); setPin(""); setErr(""); }}>
        <div style={{ padding: "30px 22px", display: "flex", flexDirection: "column", alignItems: "center", gap: 22 }}>
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 15, fontWeight: 700 }}>新しい PIN を設定</div>
          <PinDots len={pin.length} />
          <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, color: "var(--ink-soft)" }}>4〜6桁の数字</div>
          {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 12, fontWeight: 700, color: "var(--carle-red)", textAlign: "center" }}>{err}</div>}
          <div style={{ width: "100%", maxWidth: 320 }}>
            <PinKeypad onPress={pressPin} />
            <button disabled={pin.length < 4 || busy} onClick={finish} style={{ ...ctaStyle(pin.length < 4 || busy), marginTop: 14 }}>
              {busy ? "設定中…" : "PIN を再設定してログイン"}
            </button>
          </div>
        </div>
      </AuthScene>
    );
  }

  return (
    <AuthScene title="PIN 再設定" sub="メールで本人確認します" onBack={onBack}>
      <div style={{ padding: "24px 22px", display: "flex", flexDirection: "column", gap: 18 }}>
        <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, color: "var(--ink-soft)", lineHeight: 1.8 }}>
          登録済みのメールアドレスに確認コードを送り、新しい PIN を設定します。
        </div>
        <div>
          <div className="eyebrow" style={{ color: "var(--moy-deep)", marginBottom: 6 }}>MoyMoy ID</div>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 18, fontWeight: 700, color: "var(--ink-soft)" }}>@</span>
            <input value={handle} onChange={(e) => setHandle(e.target.value.replace(/^@/, ""))} placeholder="alice" maxLength={20} style={fieldStyle} />
          </div>
        </div>
        {err && <div style={{ fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)" }}>{err}</div>}
        <button disabled={!handleOk || busy} onClick={start} style={ctaStyle(!handleOk || busy)}>
          {busy ? "送信中…" : "確認コードを送る"}
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
  const [email, setEmail] = maState(null);
  const [loaded, setLoaded] = maState(false);
  const [failed, setFailed] = maState(false);
  maEffect(() => {
    if (!open) return;
    setLoaded(false);
    setFailed(false);
    let alive = true;
    MoyMoy.me()
      .then((r) => {
        if (!alive) return;
        if (r.ok) { setLinks(r.linked_mc || []); setEmail(r.email || null); }
        else setFailed(true);
      })
      .catch((e) => { console.warn("MoyMoy: settings load (MoyMoy.me) failed", e); if (alive) setFailed(true); })
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
            {email && (
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--moy-deep)", marginTop: 4 }}>
                ✓ {email}
              </div>
            )}
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
          {loaded && failed && (
            <div style={{ padding: 18, textAlign: "center", fontFamily: "var(--font-jp)", fontSize: 13, fontWeight: 700, color: "var(--carle-red)" }}>
              連携キャラクターを読み込めませんでした。<br />通信状態をご確認のうえ、開き直してください。
            </div>
          )}
          {loaded && !failed && links.length === 0 && (
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
  const [authMode, setAuthMode] = maState("welcome"); // welcome | register | login | recover
  const [adding, setAdding] = maState(false);
  const [emailEnabled, setEmailEnabled] = maState(false);

  // Whether the backend has email verification / 2FA / recovery active.
  maEffect(() => {
    let alive = true;
    MoyMoy.config()
      .then((r) => { if (alive && r && r.ok) setEmailEnabled(!!r.email_enabled); })
      .catch((e) => { console.warn("MoyMoy: /auth/config failed; email UI hidden until reload", e); });
    return () => { alive = false; };
  }, []);

  // Boot: restore the persisted session list and validate the active one.
  maEffect(() => {
    let alive = true;
    (async () => {
      const { accounts: list, activeId: aid } = await loadAccounts();
      if (!alive) return;
      const active = list.find((a) => a.account_id === aid) || list[0] || null;
      if (active) {
        MoyMoy.setSession(active.session);
        let verdict = "unknown"; // "ok" | "expired" | "unknown"
        try {
          const me = await MoyMoy.me();
          verdict = me.ok ? "ok" : (me.error === "unauthorized" ? "expired" : "unknown");
        } catch (e) {
          // network / non-200 HTTP / JSON parse failure — can't verify (transient)
          console.warn("MoyMoy: could not verify active session on boot (network/server/parse); treating as transient", e);
          verdict = "unknown";
        }
        if (!alive) return;
        if (verdict !== "expired") {
          // valid OR unverifiable (transient): keep the session. A genuine 401
          // during use is handled by the wallet's onExpired — never delete on a blip.
          setAccounts(list);
          setActiveId(active.account_id);
          setPhase("app");
          return;
        }
        // verdict === "expired" (401): session truly invalid — drop it, keep the rest.
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
    // Verify the target with a PER-CALL token so the active session is never
    // disturbed mid-verify (R05/R06) — only adopt it once verified.
    let verdict = "unknown";
    try {
      const me = await MoyMoy.me(acc.session);
      verdict = me.ok ? "ok" : (me.error === "unauthorized" ? "expired" : "unknown");
    } catch (e) {
      console.warn("MoyMoy: could not verify target account on switch (network/server/parse); aborting switch", e);
      verdict = "unknown";
    }
    if (verdict === "ok") {
      MoyMoy.setSession(acc.session);
      setActiveId(id);
      await saveAccounts(accounts, id);
      return;
    }
    if (verdict === "unknown") {
      // Couldn't verify (network/server/parse) — abort the switch. The active
      // session was never touched, so there's nothing to restore.
      return;
    }
    // verdict === "expired" (401): the target's session is invalid — drop it and
    // stay on the current (still-valid) account.
    const rest = accounts.filter((a) => a.account_id !== id);
    setAccounts(rest);
    await saveAccounts(rest, activeId);
  }

  async function logoutAccount(id) {
    const acc = accounts.find((a) => a.account_id === id);
    if (acc) {
      // Per-call token: revoke THIS account's session without clobbering the
      // active one (R05/R06).
      try {
        await MoyMoy.logout(acc.session);
      } catch (e) {
        // Best-effort server revocation: still remove the account locally, but
        // surface the failure — a silently un-revoked session stays valid server-side.
        console.warn("MoyMoy: server logout failed; removing account locally anyway", e);
      }
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
    if (authMode === "register") return <AuthRegister emailEnabled={emailEnabled} onDone={onAuthDone} onBack={() => setAuthMode("welcome")} />;
    if (authMode === "login") return <AuthLogin emailEnabled={emailEnabled} onDone={onAuthDone} onBack={() => setAuthMode("welcome")} onForgot={() => setAuthMode("recover")} />;
    if (authMode === "recover") return <AuthRecover onDone={onAuthDone} onBack={() => setAuthMode("login")} />;
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
  MoyMoyRoot, AuthWelcome, AuthRegister, AuthLogin, AuthRecover, CodeEntry,
  AccountMenu, SettingsSheet, PinKeypad, PinDots,
});
