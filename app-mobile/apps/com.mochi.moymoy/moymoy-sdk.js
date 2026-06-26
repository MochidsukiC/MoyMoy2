/* global window, fetch, location, URLSearchParams, mochi, crypto */
/* =====================================================================
   MoyMoy — client SDK (piggle-sdk.js pattern)

   Talks to the MoyMoy wallet backend at `moymoy.cs.mnn` over the MNN overlay.
   The backend is the single source of truth for balances; this SDK only frames
   requests and attaches the signed-in player's identity + an idempotency key.

   Endpoint resolution:
     - in-world (mochi-internal:// origin): https://moymoy.cs.mnn
     - browser-dev: ?moymoy_http=<base> or window.__MOYMOY_ENDPOINT__
       (default http://127.0.0.1:7433, the cs.mnn dev listen addr).

   Identity: mochi.os.gameUuid()/gameName() in-world; ?mc_uuid=/?mcid= for dev.

   REST surface (cs.mnn):
     GET  /wallet/status
     GET  /wallet/home
     GET  /wallet/history?filter=&limit=
     GET  /wallet/friends
     GET  /wallet/merchants
     GET  /wallet/inventory
     POST /wallet/send   {idem_key, from_uuid|from_mcid, to_uuid|to_mcid, amount}
     POST /wallet/pay    {idem_key, mc_uuid|mcid, merchant_id, amount}
     POST /wallet/charge {idem_key, mc_uuid|mcid, amount}
     GET  /wallet/op?op_id=
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

  // The signed-in player's identity. In-world via the OS API; in browser-dev via
  // ?mc_uuid= / ?mcid= so the app is testable without the host.
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

  function qstr(obj) {
    return Object.entries(obj)
      .filter(([, v]) => v != null && v !== "")
      .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
      .join("&");
  }

  async function getJson(path, params) {
    const i = await ident();
    const query = qstr(Object.assign({}, params || {}, i));
    const res = await fetch(base() + path + (query ? "?" + query : ""), { method: "GET" });
    if (!res.ok) throw new Error("moymoy GET " + path + " → HTTP " + res.status);
    return res.json();
  }

  async function postJson(path, body) {
    const res = await fetch(base() + path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body || {}),
    });
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
    newIdem,
    status: () => getJson("/wallet/status"),
    home: () => getJson("/wallet/home"),
    history: (filter, limit) => getJson("/wallet/history", { filter, limit }),
    friends: () => getJson("/wallet/friends"),
    merchants: () => getJson("/wallet/merchants"),
    inventory: () => getJson("/wallet/inventory"),

    // Send to a friend/player. `target` = {uuid?, mcid?}.
    send: async (target, amount) => {
      const i = await ident();
      return postJson("/wallet/send", {
        idem_key: newIdem(),
        from_uuid: i.mc_uuid,
        from_mcid: i.mcid,
        to_uuid: target.uuid || undefined,
        to_mcid: target.mcid || undefined,
        amount,
      });
    },

    // Pay a merchant by id.
    pay: async (merchantId, amount) => {
      const i = await ident();
      return postJson("/wallet/pay", {
        idem_key: newIdem(),
        mc_uuid: i.mc_uuid,
        mcid: i.mcid,
        merchant_id: merchantId,
        amount,
      });
    },

    // Charge from inventory emeralds (mod-backed). Returns a pending op; poll op().
    charge: async (amount) => {
      const i = await ident();
      return postJson("/wallet/charge", {
        idem_key: newIdem(),
        mc_uuid: i.mc_uuid,
        mcid: i.mcid,
        amount,
      });
    },

    op: (opId) => getJson("/wallet/op", { op_id: opId }),
  };
})();
