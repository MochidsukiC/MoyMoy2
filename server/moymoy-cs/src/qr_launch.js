// MoyMoy QR landing page: open the MoyMoy app on the payment this page was
// opened for. Served from the same origin as the page (CSP: script-src 'self'),
// so the page itself carries no inline script.
//
// The page shows no amount, shop or id. This script passes along only what the
// page's button carries in `data-app-params` ({"intent_id"} or {"static_qr"});
// the app fetches everything it displays from the wallet backend.
(function () {
  "use strict";
  var APP_ID = "com.mochi.moymoy";
  var button = document.getElementById("pay");
  if (!button) {
    return;
  }
  button.addEventListener("click", function () {
    var params = JSON.parse(button.getAttribute("data-app-params"));
    var apps = window.mochi && window.mochi.os && window.mochi.os.apps;
    if (apps && typeof apps.launch === "function") {
      apps.launch(APP_ID, params);
      return;
    }
    // No MochiOS shim on this page: fall back to the launch URL scheme.
    location.href =
      "mochi://apps/launch?app_id=" +
      encodeURIComponent(APP_ID) +
      "&params=" +
      encodeURIComponent(JSON.stringify(params));
  });
})();
