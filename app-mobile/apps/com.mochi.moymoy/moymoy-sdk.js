/* global window, fetch, location, URLSearchParams, mochi, crypto, localStorage */
/* =====================================================================
   MoyMoy — client SDK (piggle-sdk.js pattern)

   Talks to the MoyMoy wallet backend at `moymoy.cs.mnn` over the MNN overlay.
   The backend is the single source of truth for balances and identity.

   Identity (v2): a MoyMoy account (handle + PIN). The backend verifies every
   wallet request from the session token we send in the `X-MoyMoy-Session`
   header (minted by register/login). The Minecraft UUID (os.gameUuid) is sent
   ONLY for charge/inventory — the character whose emeralds to consume.

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
    if (window.__MOYMOY_ENDPOINT__) return trim(window.__MOYMOY_ENDPOINT__);
    const o = qs.get("moymoy_http");
    if (o) return trim(o);
    if (inWorld) return "https://moymoy.cs.mnn";
    return "http://127.0.0.1:7433"; // cs.mnn dev listen
  }

  // ── session ────────────────────────────────────────────────────────────────
  // The active account's session token, attached to every request. The app shell
  // sets/clears this as the user logs in, switches, or logs out.
  let _session = null;
  function setSession(tok) {
    _session = tok || null;
  }
  function authHeaders() {
    return _session ? { "X-MoyMoy-Session": _session } : {};
  }

  // ── Minecraft character identity (charge / inventory only) ──────────────────
  // In-world via the OS API; in browser-dev via ?mc_uuid= / ?mcid=. This is the
  // *character* (gameUuid), NOT the MoyMoy account identity.
  let _identCache = null;
  async function ident() {
    if (_identCache) return _identCache;
    let mc_uuid = "";
    let mcid = "";
    try {
      if (window.mochi && mochi.os) {
        if (mochi.os.gameUuid) mc_uuid = (await mochi.os.gameUuid()) || "";
        if (mochi.os.gameName) mcid = (await mochi.os.gameName()) || "";
      }
    } catch (e) {
      /* not in-world */
    }
    if (!mc_uuid && !mcid) {
      mc_uuid = qs.get("mc_uuid") || "";
      mcid = qs.get("mcid") || "";
    }
    _identCache = { mc_uuid, mcid };
    return _identCache;
  }

  // Device id (self-asserted metadata for the session row). Best-effort.
  async function phoneId() {
    try {
      if (window.mochi && mochi.phoneState && mochi.phoneState.get) {
        const st = await mochi.phoneState.get();
        return (st && st.phone_id) || null;
      }
    } catch (e) {
      /* not in-world */
    }
    return null;
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
          return null;
        }
      }
      try {
        return localStorage.getItem(k);
      } catch (e) {
        return null;
      }
    },
    async set(k, v) {
      if (hasMochiStorage()) {
        try {
          return await mochi.storage.set(k, v);
        } catch (e) {
          return;
        }
      }
      try {
        localStorage.setItem(k, v);
      } catch (e) {
        /* ignore */
      }
    },
    async remove(k) {
      if (hasMochiStorage()) {
        try {
          return await mochi.storage.remove(k);
        } catch (e) {
          return;
        }
      }
      try {
        localStorage.removeItem(k);
      } catch (e) {
        /* ignore */
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

  async function getJson(path, params) {
    const query = qstr(params || {});
    const res = await fetch(base() + path + (query ? "?" + query : ""), {
      method: "GET",
      headers: authHeaders(),
    });
    if (res.status === 401) return { ok: false, error: "unauthorized", status: 401 };
    if (!res.ok) throw new Error("moymoy GET " + path + " → HTTP " + res.status);
    return res.json();
  }

  async function postJson(path, body) {
    const res = await fetch(base() + path, {
      method: "POST",
      headers: Object.assign({ "Content-Type": "application/json" }, authHeaders()),
      body: JSON.stringify(body || {}),
    });
    if (res.status === 401) return { ok: false, error: "unauthorized", status: 401 };
    if (!res.ok) throw new Error("moymoy POST " + path + " → HTTP " + res.status);
    return res.json();
  }

  const newIdem = () =>
    window.crypto && crypto.randomUUID
      ? crypto.randomUUID()
      : "k-" + Date.now() + "-" + Math.floor(Math.random() * 1e9);

  window.MoyMoy = {
    inWorld,
    base,
    ident,
    phoneId,
    newIdem,
    setSession,
    store,

    // ── auth (independent MoyMoy accounts) ──
    register: async ({ handle, display_name, pin }) =>
      postJson("/auth/register", { handle, display_name, pin, phone_id: await phoneId() }),
    login: async ({ handle, pin }) =>
      postJson("/auth/login", { handle, pin, phone_id: await phoneId() }),
    logout: () => postJson("/auth/logout", {}),
    me: () => getJson("/auth/me"),
    lookup: (handle) => getJson("/auth/lookup", { handle }),

    // ── wallet (session-authenticated) ──
    status: () => getJson("/wallet/status"),
    home: () => getJson("/wallet/home"),
    history: (filter, limit) => getJson("/wallet/history", { filter, limit }),
    friends: () => getJson("/wallet/friends"),
    merchants: () => getJson("/wallet/merchants"),

    // Inventory of the current Minecraft character (charge screen).
    inventory: async () => {
      const i = await ident();
      return getJson("/wallet/inventory", { mc_uuid: i.mc_uuid, mcid: i.mcid });
    },

    // Send to a MoyMoy handle (@id).
    send: (toHandle, amount) =>
      postJson("/wallet/send", { idem_key: newIdem(), to_handle: toHandle, amount }),

    // Pay a merchant by id.
    pay: (merchantId, amount) =>
      postJson("/wallet/pay", { idem_key: newIdem(), merchant_id: merchantId, amount }),

    // Charge from the current character's inventory emeralds (mod-backed).
    // Returns a pending op; poll op().
    charge: async (amount) => {
      const i = await ident();
      return postJson("/wallet/charge", {
        idem_key: newIdem(),
        amount,
        mc_uuid: i.mc_uuid,
        mcid: i.mcid,
      });
    },

    op: (opId) => getJson("/wallet/op", { op_id: opId }),
  };
})();
