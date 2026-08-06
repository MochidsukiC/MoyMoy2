/* global window, fetch, location, URLSearchParams, mochi, crypto, localStorage */
/* =====================================================================
   MoyMoy — client SDK (piggle-sdk.js pattern)

   Talks to the MoyMoy wallet backend at `moymoy.cs.mnn` over the MNN overlay.
   The backend is the single source of truth for balances and identity.
   (One ingress, one claim: the backend's emerald-charge path is HTTP over the
   same tunnel, so the old `wallet.moymoy` / `charge.moymoy` sub-host split — a
   workaround for the command bus needing a claim of its own — is gone.)

   Identity (v2): a MoyMoy account (handle + PIN). The backend verifies every
   wallet request from the session token we send in the `X-MoyMoy-Session`
   header (minted by register/login).

   Which Minecraft character's emeralds a charge may consume is a SEPARATE
   question, and one this app cannot answer about itself — it is the thing being
   asserted about. So it no longer sends an `os.gameUuid` of its own. It asks the
   OS for a Hub-signed host attestation (`mochi.os.hostAttestation`, DEV.md
   §7.3.10 G4), which requires the user's consent and is bound to this backend
   (`audience`), to a nonce the backend issued (`challenge`) and to the exact
   request (`requestHash`). The backend verifies the signature; nothing here is
   trusted on the app's word.

   Endpoint resolution:
     - in-world (mochi-internal:// origin): https://moymoy.cs.mnn
     - browser-dev: ?moymoy_http=<base> or window.__MOYMOY_ENDPOINT__
       (default http://127.0.0.1:7433, the cs.mnn dev listen addr).

   Persistence: session list is kept by the app shell (MoyMoyRoot) via
   MoyMoy.store (mochi.storage in-world, localStorage in browser-dev).
   ===================================================================== */
(function () {
  "use strict";

  const qs = new URLSearchParams(location.search);
  const inWorld = location.protocol.indexOf("mochi-internal") === 0;
  const trim = (u) => String(u).replace(/\/+$/, "");

  function base() {
    // In-world the endpoint is ours to know, not the caller's to choose. Any
    // app can navigate us with `mochi-internal://com.mochi.moymoy/index.html?
    // moymoy_http=<theirs>` and the query survives a cold start, so reading the
    // dev override first would hand them the handle, the PIN and the session
    // token. The override stays available where it is meant to be used.
    if (inWorld) return "https://moymoy.cs.mnn";
    if (window.__MOYMOY_ENDPOINT__) return trim(window.__MOYMOY_ENDPOINT__);
    const o = qs.get("moymoy_http");
    if (o) return trim(o);
    return "http://127.0.0.1:7433"; // cs.mnn dev listen
  }

  // ── session ────────────────────────────────────────────────────────────────
  // The active account's session token, attached to every request. The app shell
  // sets/clears this as the user logs in, switches, or logs out.
  let _session = null;
  function setSession(tok) {
    _session = tok || null;
  }
  // Resolve the token for a call: an explicit per-call `session` wins over the
  // active global one. Per-call tokens let logout/switch verify a SPECIFIC
  // account without mutating the shared _session, so a concurrent wallet call
  // never sends the wrong account's header (R05/R06).
  function authHeaders(session) {
    const tok = session || _session;
    return tok ? { "X-MoyMoy-Session": tok } : {};
  }

  // Device id (self-asserted metadata for the session row). Best-effort.
  async function phoneId() {
    try {
      if (window.mochi && mochi.phoneState && mochi.phoneState.get) {
        const st = await mochi.phoneState.get();
        return (st && st.phone_id) || null;
      }
    } catch (e) {
      console.warn("MoyMoy.phoneId: phoneState API threw", e);
    }
    return null;
  }

  // Device owner's Mochi account UUID (self-asserted). Best-effort: null on
  // browser-dev (no `mochi` global), on an unsynced device (no owner yet), or
  // if the host API throws.
  async function mochiOwner() {
    try {
      if (window.mochi && mochi.phoneState && mochi.phoneState.get) {
        const st = await mochi.phoneState.get();
        return (st && st.owner) || null;
      }
    } catch (e) {
      console.warn("MoyMoy.mochiOwner: phoneState API threw", e);
    }
    return null;
  }

  // Register this device's owner so it receives incoming-payment
  // notifications. Best-effort and never blocks the caller: skips silently
  // when there is no owner to report, and only warns on failure.
  async function link(session) {
    const owner = await mochiOwner();
    if (!owner) return;
    try {
      const r = await postJson("/wallet/link", { mochi_account_id: owner }, session);
      if (!r || !r.ok) console.warn("MoyMoy.link: /wallet/link answered not-ok", r);
    } catch (e) {
      console.warn("MoyMoy.link: /wallet/link failed", e);
    }
  }

  // ── persistent storage (account/session list lives here) ────────────────────
  function hasMochiStorage() {
    return !!(window.mochi && mochi.storage && mochi.storage.get);
  }
  const store = {
    async get(k) {
      if (hasMochiStorage()) {
        try {
          return await mochi.storage.get(k);
        } catch (e) {
          console.error("MoyMoy.store.get (mochi.storage) failed", e);
          return null;
        }
      }
      try {
        return localStorage.getItem(k);
      } catch (e) {
        console.error("MoyMoy.store.get (localStorage) failed", e);
        return null;
      }
    },
    // Returns true on success, false on failure (R13 — surface persistence
    // failures instead of swallowing them, so the caller can warn the user that
    // their account list may not survive a restart).
    async set(k, v) {
      if (hasMochiStorage()) {
        try {
          await mochi.storage.set(k, v);
          return true;
        } catch (e) {
          console.error("MoyMoy.store.set (mochi.storage) failed", e);
          return false;
        }
      }
      try {
        localStorage.setItem(k, v);
        return true;
      } catch (e) {
        console.error("MoyMoy.store.set (localStorage) failed", e);
        return false;
      }
    },
    async remove(k) {
      if (hasMochiStorage()) {
        try {
          return await mochi.storage.remove(k);
        } catch (e) {
          console.error("MoyMoy.store.remove (mochi.storage) failed", e);
          return;
        }
      }
      try {
        localStorage.removeItem(k);
      } catch (e) {
        console.error("MoyMoy.store.remove (localStorage) failed", e);
      }
    },
  };

  // ── transport ───────────────────────────────────────────────────────────────
  function qstr(obj) {
    return Object.entries(obj)
      .filter(([, v]) => v != null && v !== "")
      .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
      .join("&");
  }

  // 429 is a DOMAIN answer, not a transport fault: the merchant endpoints answer
  // `{ok:false, error:"rate_limited", retry_after_ms}` with that status. Throwing
  // on it turned a legitimate "you registered a shop 3 minutes ago, wait 7 more"
  // into 「通信に失敗しました」 — reporting a cause the app had already been told
  // was something else. Handled alongside 401 for the same reason.
  async function readAnswer(res, what) {
    if (res.status === 401) return { ok: false, error: "unauthorized", status: 401 };
    if (res.status === 429) {
      try {
        return await res.json();
      } catch (e) {
        // 本文が読めない = `retry_after_ms` が分からない。既定値を置くと
        // 「あと何分待てばよいか」を推測で言うことになるので、status から確実に
        // 言える「制限された」だけを返し、待ち時間は名乗らない（errLabelFor は
        // retry_after_ms が無ければ時間を出さない）。読めなかった事実は残す。
        console.error("moymoy " + what + " → HTTP 429 but the body did not parse", e);
        return { ok: false, error: "rate_limited", status: 429 };
      }
    }
    if (!res.ok) throw new Error("moymoy " + what + " → HTTP " + res.status);
    return res.json();
  }

  async function getJson(path, params, session) {
    const query = qstr(params || {});
    const res = await fetch(base() + path + (query ? "?" + query : ""), {
      method: "GET",
      headers: authHeaders(session),
    });
    return readAnswer(res, "GET " + path);
  }

  async function postJson(path, body, session) {
    const res = await fetch(base() + path, {
      method: "POST",
      headers: Object.assign({ "Content-Type": "application/json" }, authHeaders(session)),
      body: JSON.stringify(body || {}),
    });
    return readAnswer(res, "POST " + path);
  }

  const newIdem = () =>
    window.crypto && crypto.randomUUID
      ? crypto.randomUUID()
      : "k-" + Date.now() + "-" + Math.floor(Math.random() * 1e9);

  // ── launch intent (OS → this app) ───────────────────────────────────────────
  /**
   * Reduce one OS launch intent to the only field this app is willing to act on.
   *
   * `params` is whatever the LAUNCHING app chose to put there, and it is not
   * trusted for anything: only `intent_id` is lifted out, every other key is
   * dropped unread. The approval screen's amount, merchant and description come
   * from `GET /wallet/payment/intent` — the backend's own record — so a shop
   * cannot make the screen say one thing while billing another (WYSIWYS).
   *
   * `from` and `age_ms` DO come from here, and only from here: the OS stamps
   * `from` with the attested sender id, which is the one thing about the caller
   * that cannot be forged.
   */
  function readLaunchIntent(raw) {
    if (!raw || typeof raw !== "object") return null;
    const params = raw.params;
    const id = params && typeof params.intent_id === "string" ? params.intent_id.trim() : "";
    // Prefix and length only. Deliberately NOT a charset rule: `payments.rs`
    // mints `pi_` + URL-safe base64, and pinning the alphabet here would couple
    // the app to an encoding it does not own — the day that changes, every
    // payment would vanish silently, because a rejected id produces no screen
    // and no error. Whether an id exists is the backend's call (`unknown_intent`),
    // and `qstr` escapes whatever we pass.
    if (id.indexOf("pi_") !== 0 || id.length > 128) return null;
    const from = typeof raw.from === "string" ? raw.from : "";
    const age = Number(raw.age_ms);
    return { intent_id: id, from, age_ms: Number.isFinite(age) && age >= 0 ? age : 0 };
  }

  // ── host attestation ────────────────────────────────────────────────────────
  // Who we are asking the assertion to be FOR. The backend refuses one minted
  // for anybody else, so this string must match its `attest::AUDIENCE`.
  const AUDIENCE = "moymoy";

  /**
   * The platform's request binding: SHA-256, lower-case hex, over the domain
   * followed by each part, every field framed as `<utf8_byte_len>:<field>`.
   * `requestHash("d", ["ab", "c"])` hashes `"1:d2:ab1:c"`.
   *
   * This is `mochi_proto_attest::request_hash`, and it MUST agree byte for byte
   * or nothing verifies. Two things to leave alone:
   *  - the length is `encode(s).length` (UTF-8 bytes), never `s.length` (UTF-16
   *    units) — they differ on any non-ASCII field;
   *  - the framing is what makes the digest input parse back to exactly one field
   *    list, so no value a caller passes can move a boundary. A plain separator
   *    would let a field containing it read as two fields — a different request
   *    approved by the same signature.
   */
  async function requestHash(domain, parts) {
    const enc = new TextEncoder();
    const framed = [domain].concat(parts).map((s) => enc.encode(s).length + ":" + s).join("");
    const digest = await crypto.subtle.digest("SHA-256", enc.encode(framed));
    return Array.from(new Uint8Array(digest))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
  }
  // The bindings, spelled exactly as the backend spells them
  // (server/moymoy-cs/src/attest.rs). The distinct DOMAINS are why an assertion
  // approved for a character check can never be spent as one that authorizes a
  // charge, and why a charge assertion can never be replayed as a withdrawal (or
  // vice versa) — each hash space is disjoint from the others, so consent given
  // for one direction of money movement cannot be spent on the other.
  const chargeRequestHash = (idemKey, amount) =>
    requestHash("moymoy.charge.v1", [idemKey, String(amount)]);
  const withdrawRequestHash = (idemKey, amount) =>
    requestHash("moymoy.withdraw.v1", [idemKey, String(amount)]);
  // No parts: a confirmation binds nothing but its purpose. The challenge is what
  // makes it unreplayable, and it is inside the signed claims.
  const sessionRequestHash = () => requestHash("moymoy.session.v1", []);

  /**
   * Get the backend a fresh nonce, then ask the OS to attest this host against
   * it. Resolves `{ok:true, assertion}` or `{ok:false, error:<code>}` — the
   * reason is always reported as its own code rather than collapsed into a
   * generic failure, so the UI can say what actually happened.
   *
   * `makeRequestHash()` returns the digest that binds the assertion to the
   * request it will authorize. It takes no challenge: the backend cannot read the
   * challenge out of an assertion before its policy check has passed, so the
   * binding is over the request's own content (or, for a confirmation, over
   * nothing but the purpose).
   */
  async function attest(purpose, makeRequestHash, reason) {
    // Browser-dev has no OS, hence no session credential to redeem: charging is
    // in-world only, by design (a dev bypass would be a way to spend somebody
    // else's emeralds without ever proving anything).
    if (!(window.mochi && mochi.os && mochi.os.hostAttestation)) {
      return { ok: false, error: "attest_no_host" };
    }
    let ch;
    try {
      ch = await postJson("/wallet/attest/challenge", { purpose });
    } catch (e) {
      console.warn("MoyMoy.attest: challenge request failed", e);
      return { ok: false, error: "attest_challenge" };
    }
    if (!ch.ok || !ch.challenge) {
      return { ok: false, error: ch.error || "attest_challenge" };
    }

    let hash;
    try {
      hash = await makeRequestHash();
    } catch (e) {
      console.error("MoyMoy.attest: could not compute the request hash", e);
      return { ok: false, error: "attest_no_crypto" };
    }

    let res;
    try {
      res = await mochi.os.hostAttestation({
        audience: AUDIENCE,
        challenge: ch.challenge,
        requestHash: hash,
        reason,
      });
    } catch (e) {
      // A rejection is an OS-level fault, not an answer. The overwhelmingly
      // likely one is a manifest missing `attestation.request`, so name it.
      if (e && e.code === "SCOPE_DENIED") {
        console.error("MoyMoy.attest: the app manifest does not declare attestation.request", e);
        return { ok: false, error: "attest_no_scope" };
      }
      console.error("MoyMoy.attest: os.hostAttestation failed", e);
      return { ok: false, error: "attest_os_error" };
    }
    const status = res && res.status;
    if (status === "ok" && res.assertion) return { ok: true, assertion: res.assertion };
    if (status === "denied") return { ok: false, error: "attest_denied" };
    if (status === "timeout") return { ok: false, error: "attest_timeout" };
    if (status === "unavailable") return { ok: false, error: "attest_unavailable" };
    console.warn("MoyMoy.attest: unexpected attestation outcome", res);
    return { ok: false, error: "attest_os_error" };
  }

  window.MoyMoy = {
    inWorld,
    base,
    phoneId,
    link,
    newIdem,
    setSession,
    store,

    // ── auth (independent MoyMoy accounts) ──
    // Whether email verification / 2FA / recovery are active (SMTP configured).
    config: () => getJson("/auth/config"),
    // register: with email enabled → {pending:"verify_email"}; else → {session}.
    register: async ({ handle, display_name, pin, email }) =>
      postJson("/auth/register", { handle, display_name, pin, email, phone_id: await phoneId() }),
    registerVerify: async ({ email, code }) =>
      postJson("/auth/register/verify", { email, code, phone_id: await phoneId() }),
    // login: with 2FA → {pending:"2fa"}; else → {session}.
    login: async ({ handle, pin }) =>
      postJson("/auth/login", { handle, pin, phone_id: await phoneId() }),
    loginVerify: async ({ handle, code }) =>
      postJson("/auth/login/verify", { handle, code, phone_id: await phoneId() }),
    // PIN recovery via an emailed code.
    recoverStart: ({ handle }) => postJson("/auth/recover/start", { handle }),
    recoverVerify: async ({ handle, code, new_pin }) =>
      postJson("/auth/recover/verify", { handle, code, new_pin, phone_id: await phoneId() }),
    // logout/me accept an optional per-call session so the app shell can act on
    // a SPECIFIC account (switch-verify, logout-other) without disturbing the
    // active session (R05/R06).
    logout: (session) => postJson("/auth/logout", {}, session),
    me: (session) => getJson("/auth/me", null, session),
    lookup: (handle) => getJson("/auth/lookup", { handle }),

    // ── wallet (session-authenticated) ──
    status: () => getJson("/wallet/status"),
    home: () => getJson("/wallet/home"),
    history: (filter, limit) => getJson("/wallet/history", { filter, limit }),
    friends: () => getJson("/wallet/friends"),
    merchants: () => getJson("/wallet/merchants"),

    // Confirm which character is playing, so the backend has somewhere to send
    // an inventory query. Raises the OS consent modal — call it only from a user
    // action, never on load.
    attestSession: async (reason) => {
      const a = await attest("session", sessionRequestHash, reason || "Minecraft のキャラクターを確認します");
      if (!a.ok) return { ok: false, error: a.error };
      return postJson("/wallet/attest/session", { assertion: a.assertion });
    },

    // Inventory of the confirmed character. Takes no arguments and NEVER attests
    // on its own: it is called on tab switches and refreshes, and a modal at
    // those moments would be the phone asking for consent unprompted. An
    // `attestation_required` answer is the app's cue to offer the button.
    inventory: () => getJson("/wallet/inventory"),

    // Send to a MoyMoy handle (@id). Pass a stable `idemKey` so a retry of the
    // SAME send (e.g. after a lost response) replays the same transfer instead of
    // sending twice.
    //
    // `pin` is absent on the first attempt on purpose: riskauth decides from the
    // amount whether one is needed, and answers `pin_required` when it is. Asking
    // for a PIN up front would charge every 20 エメ send the friction of a
    // 20,000 エメ one. The retry MUST carry the same `idemKey`.
    send: (toHandle, amount, idemKey, pin, otp) =>
      postJson("/wallet/send", { idem_key: idemKey || newIdem(), to_handle: toHandle, amount, pin, otp }),

    // Mail a step-up code to the account's verified address. Its own OTP purpose,
    // so a code the user asked for to confirm a payment cannot be replayed as a
    // login — and a login code cannot be spent here either.
    //
    // `{ok:false,error:"too_soon",retry_after_ms}` means a code was sent moments
    // ago and is still valid, NOT that sending failed. A 5xx means the mail
    // itself did not go out; the server rolls the code back, so a retry is safe
    // and does not have to wait out the cooldown.
    stepupOtp: () => postJson("/wallet/stepup/otp", {}),

    // ── EC payment, payer side ──
    // The approval screen's ONLY source. Everything it renders comes from here,
    // never from the launch params (see readLaunchIntent).
    paymentIntent: (intentId) => getJson("/wallet/payment/intent", { intent_id: intentId }),

    // Approve and pay. No `idem_key`: the intent row IS the idempotency record —
    // the backend rebuilds the same response from `state`/`payer`/`tx_id`, so a
    // retry after a lost response replays server-side. Minting a key here would
    // add a second, weaker notion of "the same payment".
    paymentApprove: (intentId, pin, otp) =>
      postJson("/wallet/payment/approve", { intent_id: intentId, pin, otp }),

    // Refuse it. No PIN: nothing moves, and making somebody authenticate to say
    // "no" is how people are taught to type their PIN into whatever asks.
    paymentDecline: (intentId) => postJson("/wallet/payment/decline", { intent_id: intentId }),

    // ── merchant portal (session + PIN) ──
    // The credential-issuing half. Separate from the API-key half on purpose: a
    // leaked key cannot reach any of these, and a stolen session cannot use them
    // without the PIN.
    merchantList: () => getJson("/merchant/portal/list"),
    merchantRegister: ({ name, sub, pin }) =>
      postJson("/merchant/portal/register", { name, sub, pin }),
    // Both of these answer with a NEW plaintext key, shown once and never stored.
    merchantRotateKey: (merchantId, pin) =>
      postJson("/merchant/portal/key", { merchant_id: merchantId, pin }),
    merchantSetStatus: (merchantId, status, pin) =>
      postJson("/merchant/portal/status", { merchant_id: merchantId, status, pin }),
    merchantSetLimits: (merchantId, pin, limits) =>
      postJson("/merchant/portal/limits", {
        merchant_id: merchantId,
        pin,
        max_open_intents: limits ? limits.max_open_intents : undefined,
        daily_issue_cap: limits ? limits.daily_issue_cap : undefined,
      }),

    // ── OS launch intent ──
    // Consume the payload another app launched us with. Single-use: a second call
    // reads null, which is what makes it safe to call on every boot AND from the
    // onLaunchIntent push without re-running an action the user already answered.
    takeLaunchIntent: async () => {
      if (!(window.mochi && mochi.os && mochi.os.apps && mochi.os.apps.takeLaunchIntent)) return null;
      try {
        return readLaunchIntent(await mochi.os.apps.takeLaunchIntent());
      } catch (e) {
        // An OS-level rejection is not "no intent pending" — say so rather than
        // letting a broken bridge look like an ordinary empty mailbox.
        console.error("MoyMoy: os.apps.takeLaunchIntent failed", e);
        return null;
      }
    },
    // Register for the warm-switch push. The host's notification carries no
    // payload — it only says "call takeLaunchIntent again" — and there is no way
    // to unregister, so `cb` must be safe to run at any time.
    onLaunchIntent: (cb) => {
      if (window.mochi && mochi.os && mochi.os.apps && mochi.os.apps.onLaunchIntent) {
        mochi.os.apps.onLaunchIntent(cb);
      }
    },
    // Go back to the app that launched us. The id passed here must be the OS's
    // attested `from`, never a merchant-declared `return_app_id` / `launch_app_id`
    // — those are self-reported and would make the shop the authority on where
    // the user lands after paying.
    launchApp: async (appId) => {
      if (!(window.mochi && mochi.os && mochi.os.apps && mochi.os.apps.launch)) {
        return { accepted: false, reason: "no_host" };
      }
      try {
        return await mochi.os.apps.launch(appId);
      } catch (e) {
        if (e && e.code === "SCOPE_DENIED") {
          console.error("MoyMoy: the app manifest does not declare apps.launch", e);
          return { accepted: false, reason: "no_scope" };
        }
        console.error("MoyMoy: os.apps.launch failed", e);
        return { accepted: false, reason: "os_error" };
      }
    },

    // Charge from the confirmed character's inventory emeralds (mod-backed).
    // Returns a pending op; poll op(). Pass a stable `idemKey` so a retry of the
    // SAME charge attempt replays the same op instead of consuming twice.
    //
    // Posts WITHOUT an assertion first, on purpose: a retry of an already-created
    // op replays server-side and comes back immediately, so the user is not asked
    // to approve a charge they already approved. Only a genuinely new charge gets
    // `attestation_required` back, and only then is the consent modal raised.
    charge: async (amount, idemKey) => {
      const idem = idemKey || newIdem();
      const first = await postJson("/wallet/charge", { idem_key: idem, amount });
      if (first.ok || first.error !== "attestation_required") return first;

      // The charge binding covers the idem_key and the amount, not the
      // challenge — so the modal's reason can state the amount the user is about
      // to approve, and an assertion for a different amount will not verify.
      // amount は minor (1/100 エメ)。署名対象のハッシュは minor の生値を
      // そのまま使う（chargeRequestHash に変換を挟まない）が、ユーザーに
      // 見せる同意文言は ÷100 して「エメ」で言う — 署名される値とユーザーが
      // 見る額を一致させるため。
      const a = await attest(
        "charge",
        () => chargeRequestHash(idem, amount),
        (amount / 100).toFixed(2) + " エメのチャージ"
      );
      if (!a.ok) return { ok: false, error: a.error };
      return postJson("/wallet/charge", { idem_key: idem, amount, assertion: a.assertion });
    },

    // Withdraw into the confirmed character's inventory (mod-backed): the
    // reverse of charge, moving balance out and minting emeralds/blocks
    // in-world. Returns a pending op; poll op(). Pass a stable `idemKey` so a
    // retry of the SAME withdrawal attempt replays the same op instead of
    // paying out twice.
    //
    // Posts WITHOUT an assertion first, for the same reason as charge: a retry
    // of an already-created op replays server-side and comes back immediately,
    // so the user is not asked to approve a withdrawal they already approved.
    // Only a genuinely new withdrawal gets `attestation_required` back.
    //
    // Uses its OWN assertion domain (purpose "withdraw", requestHash domain
    // "moymoy.withdraw.v1") — never the charge one. A charge assertion says
    // "I consent to spend my in-world emeralds"; it must not also be usable to
    // authorize the opposite operation, paying emeralds back OUT to the world.
    // `pin` rides along for the same reason it does on send: a withdrawal is an
    // outflow, so riskauth gates it, and anything over the frictionless band
    // answers `pin_required`. The caller collects one and retries with the SAME
    // idemKey.
    withdraw: async (amount, idemKey, pin, otp) => {
      const idem = idemKey || newIdem();
      const first = await postJson("/wallet/withdraw", { idem_key: idem, amount, pin, otp });
      if (first.ok || first.error !== "attestation_required") return first;

      // charge と同じ理由: amount (minor) はハッシュへは無変換で渡し、
      // 同意文言だけ ÷100 して表示する。
      const a = await attest(
        "withdraw",
        () => withdrawRequestHash(idem, amount),
        (amount / 100).toFixed(2) + " エメの出金"
      );
      if (!a.ok) return { ok: false, error: a.error };
      return postJson("/wallet/withdraw", { idem_key: idem, amount, pin, otp, assertion: a.assertion });
    },

    op: (opId) => getJson("/wallet/op", { op_id: opId }),
  };
})();
