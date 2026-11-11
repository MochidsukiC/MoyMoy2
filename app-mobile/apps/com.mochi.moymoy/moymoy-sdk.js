/* global window, fetch, location, URL, URLSearchParams, mochi, crypto, localStorage */
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
  //
  // `domainErrors` is opt-in and only the QR / fulfil endpoints pass it. Those
  // answer a refusal with a 4xx whose body is the reason (`already_fulfilled`
  // 409, `bad_fulfilled_amount` 400, ...), and the default branch below throws on
  // every non-OK status without reading the body — which would turn each of
  // them into 「通信に失敗しました」 and lose the one fact the caller needs. With
  // the flag set, a 4xx whose body is `{ok:false, error:<string>}` is returned
  // as that answer; anything else (a 5xx, an HTML error page, a body that is not
  // that shape) still throws exactly as before. Existing callers do not pass it,
  // so their behavior is unchanged.
  async function readAnswer(res, what, domainErrors) {
    if (res.status === 401) return { ok: false, error: "unauthorized", status: 401 };
    if (domainErrors && res.status >= 400 && res.status < 500 && res.status !== 429) {
      let body = null;
      try {
        body = await res.json();
      } catch (e) {
        console.error("moymoy " + what + " → HTTP " + res.status + " but the body did not parse", e);
      }
      if (body && body.ok === false && typeof body.error === "string") {
        return Object.assign({}, body, { status: res.status });
      }
      throw new Error("moymoy " + what + " → HTTP " + res.status);
    }
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

  async function getJson(path, params, session, domainErrors) {
    const query = qstr(params || {});
    const res = await fetch(base() + path + (query ? "?" + query : ""), {
      method: "GET",
      headers: authHeaders(session),
    });
    return readAnswer(res, "GET " + path, domainErrors);
  }

  async function postJson(path, body, session, domainErrors) {
    const res = await fetch(base() + path, {
      method: "POST",
      headers: Object.assign({ "Content-Type": "application/json" }, authHeaders(session)),
      body: JSON.stringify(body || {}),
    });
    return readAnswer(res, "POST " + path, domainErrors);
  }

  const newIdem = () =>
    window.crypto && crypto.randomUUID
      ? crypto.randomUUID()
      : "k-" + Date.now() + "-" + Math.floor(Math.random() * 1e9);

  // ── launch intent (OS → this app) ───────────────────────────────────────────
  // Same number as `com.mochi.browser`'s `MAX_LAUNCH_URL_LEN`, and it has to be:
  // the browser re-applies that cap to `params.url` when we hand the value back,
  // so a longer URL is refused over there no matter what we decide here. Keeping
  // one here that is any wider would only buy a return button that does nothing
  // when it is pressed.
  //
  // It is measured on the NORMALIZED href, because that is the string the
  // browser measures — see the check itself. This is the only length rule this
  // app has; the raw bound below is a guard, not a second spec.
  const MAX_RETURN_URL_LEN = 2048;
  // A bound on the work `new URL` is asked to do, deliberately set where it
  // cannot also become a rule about which URLs are acceptable. The OS caps a
  // launch intent's whole `params` JSON at 4096 BYTES, so no value that reaches
  // us through the OS can exceed this many chars (non-ASCII costs more bytes
  // than chars, never fewer) — which is the point: the guard never fires on a
  // real intent, and every real decision is made after parsing.
  //
  // Setting it to 2048 instead would quietly make the raw string a second,
  // stricter rule than the one the contract states. Normalization SHRINKS URLs
  // as often as it grows them (a default port dropped, `/a/./b/../c` collapsed,
  // tabs removed), so a raw-2049 URL that normalizes to 17 chars — which the
  // browser accepts — would be refused here for a length it does not have by
  // the time anyone measures it.
  const MAX_RAW_RETURN_URL_LEN = 4096;

  /**
   * Reduce a merchant-supplied `return_url` to a page this app may send the
   * payer back to, or "" when there is none it is allowed to use.
   *
   * `origin` is the OS's attestation of which `.mnn` page started this launch
   * (`docs/mobile/web-launch.md`) — the one field of a web-started intent the
   * page cannot write for itself — and every rule below is a comparison against
   * it. The whole point is that a shop may return the payer INTO ITS OWN SITE
   * and nowhere else: a shop that could name any URL could drop a just-paid user
   * on a page of its choosing, which is where a convincing fake receipt (or a
   * "your payment failed, pay again here") would live.
   *
   * The value is never rendered and never reaches the approval screen. It is a
   * destination only, and it goes straight back to the OS.
   */
  function webReturnUrl(value, origin) {
    // No attested origin means this was an app→app launch, and there is nothing
    // to hold the URL against. Refuse to carry one AT ALL rather than fall back
    // to a looser check: the origin match IS the guarantee, so without an origin
    // there is no guarantee left to make — and accepting it anyway would let any
    // app that can launch us, with no web page and no host attestation anywhere
    // in the chain, name an arbitrary URL and have us ask the browser to open it.
    if (typeof origin !== "string" || origin === "") return "";
    if (typeof value !== "string") return "";
    const s = value.trim();
    if (!s) return "";
    if (s.length > MAX_RAW_RETURN_URL_LEN) {
      console.warn("MoyMoy: return_url refused (too long to parse) — origin " + origin);
      return "";
    }
    let parsed;
    let attested;
    try {
      // No base URL on purpose: a relative reference has no meaning across an
      // app boundary, and `new URL` rejects it (as it does `http://` with no
      // host). `origin` goes through the SAME parser so the host comparison
      // below is between two values normalized identically.
      parsed = new URL(s);
      attested = new URL(origin);
    } catch {
      // The exception is deliberately not bound and never printed. A `TypeError`
      // from `new URL` is free to quote the input it choked on (engines differ,
      // and nothing in the spec forbids it), so logging `e.message` would hand
      // the console the very `user:password@host` this gate exists to refuse —
      // the same leak, arriving through the exception instead of the value.
      console.warn("MoyMoy: return_url refused (not an absolute URL) — origin " + origin);
      return "";
    }
    // `URL` already lower-cases the scheme; comparing the lower-cased value
    // keeps the rule ("exactly http: or https:, case-insensitive") true of the
    // input. Anything else — `javascript:`, `mochi-internal:` — is refused here
    // and would be refused by the browser too.
    const scheme = String(parsed.protocol).toLowerCase();
    if (scheme !== "http:" && scheme !== "https:") {
      console.warn("MoyMoy: return_url refused (scheme is not http/https) — origin " + origin);
      return "";
    }
    // Userinfo, tested on the RAW authority and not on `parsed.username` /
    // `parsed.password` alone. Those two are "" for an EMPTY userinfo
    // (`https://@evil.mnn/`, `https://:@evil.mnn/`) and the parser then drops
    // the `@` from `href` entirely — so a username/password test by itself
    // accepts an authority the sender deliberately wrote an `@` into and hands
    // back a URL that is not the string that was sent. `com.mochi.browser`
    // refuses that exact shape for the same reason, so anything we let through
    // here it would stop anyway, and the payer would be left pressing a return
    // button that silently does nothing.
    //
    // Slice the authority out of the trimmed input: everything after the
    // scheme's `:` and its leading slashes, up to the first `/`, `\`, `?` or `#`
    // (WHATWG treats `\` as `/` for http(s), and `http:/host/` is a valid
    // spelling of the same authority — which is why this cuts at the scheme
    // colon rather than at `://`). An `@` in the PATH —
    // `http://a.mnn/mail/user@example` — is untouched: the slice stops at the
    // first separator.
    const afterScheme = s.slice(s.indexOf(":") + 1).replace(/^[/\\]+/, "");
    const sepAt = afterScheme.search(/[/\\?#]/);
    const authority = sepAt < 0 ? afterScheme : afterScheme.slice(0, sepAt);
    if (authority.indexOf("@") >= 0 || parsed.username !== "" || parsed.password !== "") {
      console.warn("MoyMoy: return_url refused (userinfo in authority) — origin " + origin);
      return "";
    }
    if (!parsed.hostname) {
      console.warn("MoyMoy: return_url refused (empty host) — origin " + origin);
      return "";
    }
    // The rule itself: the same HOST as the page the OS says launched us.
    // Deliberately NOT the same scheme and NOT the same port — a host is a
    // `.mnn` name with an owner, and which scheme or port that owner serves its
    // own checkout page on is the owner's business, not ours; requiring a match
    // there would break a shop that redirects `http` → `https` without stopping
    // anything, since both spellings belong to the same person either way.
    //
    // Both sides come out of the same `URL` parser. Comparing a parsed hostname
    // against a hand-sliced one is how a normalization mismatch (an IDN form, a
    // bracketed IPv6 literal) turns into a hole; WHATWG has already lower-cased
    // both, and `toLowerCase` is belt and braces.
    //
    // The root dot is the one place where the same parser is NOT enough, because
    // the attested side has been through a second normalizer this one does not
    // replicate: the host gate accepts `store.mnn.` (`host_is_mnn` trims the
    // dot) and `validate_web_origin` then runs the host through
    // `MochiDomain::parse`, whose `canonical_host` strips it — while WHATWG
    // keeps it. Without folding it here, a page served at
    // `http://store.mnn./order` that returns `location.href` (the idiom
    // `docs/mobile/web-launch.md` documents) would compare `store.mnn.` against
    // `store.mnn` and lose its return button over a spelling both sides already
    // agree is one host. One dot, matching `canonical_host` — nothing further,
    // since no host either parser mints ends in two.
    const rootHost = (h) => h.toLowerCase().replace(/\.$/, "");
    if (rootHost(parsed.hostname) !== rootHost(attested.hostname)) {
      console.warn("MoyMoy: return_url refused (host is not the launching origin) — origin " + origin);
      return "";
    }
    // `href`, not the raw input: the browser gates on `parsed.href` as well, so
    // returning the same normalization means the URL we accepted is exactly the
    // one it will act on.
    const href = parsed.href;
    // Which is also why the cap is measured HERE and not only on the input.
    // `new URL` percent-encodes raw non-ASCII, and that is a NINEFOLD growth for
    // Japanese (`あ` → `%E3%81%82`), so a 300-character order page with a product
    // name in the query hands back a 2,700-character href: accepted here,
    // refused over there for length, and the payer presses a return button that
    // does nothing. Measuring the input alone is measuring a value nobody acts
    // on. The pre-parse check stays as a cheap bound before `new URL`; this is
    // the one that has to hold. `href` is all-ASCII, so its length is also its
    // byte length, which is the unit the browser side counts in.
    if (href.length > MAX_RETURN_URL_LEN) {
      console.warn("MoyMoy: return_url refused (too long once normalized) — origin " + origin);
      return "";
    }
    return href;
  }

  /**
   * Reduce one OS launch intent to the only fields this app is willing to act on.
   *
   * `params` is whatever the LAUNCHING app chose to put there, and next to none
   * of it is trusted: `intent_id` (or `static_qr`) and `return_url` are lifted
   * out, every other key is dropped unread. The approval screen's amount,
   * merchant and description come from `GET /wallet/payment/intent` — the
   * backend's own record — so a shop cannot make the screen say one thing while
   * billing another (WYSIWYS), and `return_url` never reaches the screen at all:
   * it is checked against `origin` and then only ever handed back to the OS.
   *
   * Two kinds of launch are accepted, told apart by which key is present:
   *  - `{intent_id}`  → `kind:"intent"`: a payment the shop already created.
   *  - `{static_qr}`  → `kind:"static_qr"`: the code printed on a shop-side QR
   *    (`/qr/s/<code>`). It carries no amount; the payer types one, and the
   *    backend turns code + amount into an intent that goes through the same
   *    approval screen. The code is only a lookup key — the shop's name on
   *    screen comes from `GET /wallet/qr/static`, never from here.
   * Both keys present is ambiguous and is dropped (null) rather than guessed at.
   *
   * `from`, `origin` and `age_ms` DO come from here, and only from here: the OS
   * stamps `from` with the attested sender id and `origin` with the attested
   * origin of the `.mnn` page behind a web-started launch — the two things about
   * the caller that cannot be forged.
   */
  function readLaunchIntent(raw) {
    if (!raw || typeof raw !== "object") return null;
    const params = raw.params;
    const id = params && typeof params.intent_id === "string" ? params.intent_id.trim() : "";
    const staticQr = params && typeof params.static_qr === "string" ? params.static_qr.trim() : "";
    // Neither key, or both: nothing this app can act on, or no way to know which
    // the sender meant. Safe side — no screen, same as an unrelated launch.
    if ((id === "") === (staticQr === "")) return null;
    // Prefix and length only. Deliberately NOT a charset rule: `payments.rs`
    // mints `pi_` + URL-safe base64, and pinning the alphabet here would couple
    // the app to an encoding it does not own — the day that changes, every
    // payment would vanish silently, because a rejected id produces no screen
    // and no error. Whether an id exists is the backend's call (`unknown_intent`),
    // and `qstr` escapes whatever we pass. A static QR code (`sq_…`) is held to
    // the same rule for the same reason (`unknown_static_qr` is the backend's).
    const kind = id !== "" ? "intent" : "static_qr";
    const key = id !== "" ? id : staticQr;
    if (key.indexOf(kind === "intent" ? "pi_" : "sq_") !== 0 || key.length > 128) return null;
    const from = typeof raw.from === "string" ? raw.from : "";
    // Attested by the host, so it is carried through exactly as stamped. Absent
    // on an app→app launch, and that absence is load-bearing rather than a gap:
    // it is what makes `webReturnUrl` refuse every return URL on that path, so
    // the app→app flow keeps behaving exactly as it did before web launches
    // existed (`docs/mobile/web-launch.md`).
    const origin = typeof raw.origin === "string" ? raw.origin : "";
    const age = Number(raw.age_ms);
    // A `return_url` that does not check out collapses to "" and is never a
    // reason to drop the intent: the payment is between the payer and the
    // backend and stands with or without a way back to the page that started it.
    // Losing the return button is the entire consequence.
    const returnUrl = webReturnUrl(params && params.return_url, origin);
    const common = {
      from,
      origin,
      return_url: returnUrl,
      age_ms: Number.isFinite(age) && age >= 0 ? age : 0,
    };
    return kind === "intent"
      ? Object.assign({ kind: "intent", intent_id: id }, common)
      : Object.assign({ kind: "static_qr", static_qr: staticQr }, common);
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

    // ── static (shop-side) QR, payer side ──
    // The shop's face for a scanned code. Same fields and meaning as the
    // `merchant` part of `paymentIntent`, so the same component draws it. The
    // code from the launch params is only a lookup key; what is shown is this.
    staticQrInfo: (code) => getJson("/wallet/qr/static", { code }, undefined, true),

    // Turn a static QR + a payer-typed amount (minor units) into an ordinary
    // intent, then approve it through `paymentIntent` / `paymentApprove`.
    // Pass a stable `idemKey` per (code, amount) attempt so a retry after a lost
    // response replays the same intent instead of minting a second one; a
    // DIFFERENT amount must use a new key.
    staticQrIntent: (code, amountMinor, idemKey) =>
      postJson("/wallet/qr/static/intent",
        { code, amount_minor: amountMinor, idem_key: idemKey || newIdem() }, undefined, true),

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
    // Sales + escrow status. Read-only, session-only (no PIN) — same trust
    // level as merchantList. `limit` is optional; the server clamps it
    // (default 50 / max 200) and echoes the effective value back.
    merchantSales: (merchantId, limit) =>
      getJson("/merchant/portal/sales", { merchant_id: merchantId, limit }),
    // (Re)issue the shop's static QR. Session + PIN. Re-issuing invalidates the
    // old code, so callers must say so before asking for the PIN.
    portalStaticQr: (merchantId, pin) =>
      postJson("/merchant/portal/static-qr", { merchant_id: merchantId, pin }, undefined, true),
    // Report that the goods were handed over (all of the amount, or part of it).
    // Session only — NO PIN, like `merchantSales`: the report can only lower what
    // the shop receives, never raise it. `reason` is optional (<= 120 chars).
    portalFulfill: (merchantId, intentId, fulfilledAmountMinor, reason) =>
      postJson("/merchant/portal/fulfill", {
        merchant_id: merchantId,
        intent_id: intentId,
        fulfilled_amount_minor: fulfilledAmountMinor,
        reason: reason || undefined,
      }, undefined, true),

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
    //
    // `returnUrl` picks WHICH PAGE inside that app, and the same rule holds one
    // level down: the only value that may be passed here is one `readLaunchIntent`
    // has already held against the OS-attested `origin` (`webReturnUrl`), which
    // is what keeps a shop confined to its own host. Callers do not get to widen
    // this into a general params channel — exactly one key, `url`, is forwarded,
    // so nothing else a caller happens to be holding can ride along to the OS.
    // Omit it (or pass "") and this is the byte-for-byte call it has always been.
    launchApp: async (appId, returnUrl) => {
      if (!(window.mochi && mochi.os && mochi.os.apps && mochi.os.apps.launch)) {
        return { accepted: false, reason: "no_host" };
      }
      try {
        return typeof returnUrl === "string" && returnUrl !== ""
          ? await mochi.os.apps.launch(appId, { url: returnUrl })
          : await mochi.os.apps.launch(appId);
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
