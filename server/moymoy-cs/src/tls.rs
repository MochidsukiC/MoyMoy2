//! TLS for the `moymoy.cs.mnn` endpoint (path C: end-to-end through the
//! gateway CONNECT tunnel — the gateway never decrypts, MochiOS2.0 DEV.md
//! §5.4.1, the third of its "intake 3 形").
//!
//! **Not §7.3.3.** That section is the privileged command bus (App backend ⇄ CS
//! ⇄ MC receiving Mod, over QUIC), and the "dumb pipe" in it is that bus's
//! routing header — `command_bus.rs`. Different mechanism, different section.
//! Both halves of this module are §5.4.1: the CONNECT intake above and the
//! `.mnn` server CA below.
//!
//! MoyMoy presents its own TLS certificate — the certificate comes from the
//! Hub's `.mnn` CA (MochiOS2.0 DEV.md §5.4.1): the launcher issues one for
//! every backend it spawns and files it under `app_tls_dir`, and
//! `mochi_hub_cs_sdk::cs_tls` reads it back. MoyMoy holds no PKI code of its
//! own. The domain passed to [`server_config`] (`MOYMOY_CS_MNN`, default
//! `moymoy.cs.mnn`) must spell the SAME `.mnn` name as `domain` in
//! `run/app_backends/moymoy/app.toml` on the Hub side — that is the only
//! place the two ends agree on it, and a mismatch surfaces as a clear error
//! naming the path that was consulted (see [`cs_tls::server_config`]'s docs),
//! not a silent failure.
//!
//! # What this replaced
//!
//! This module used to mint a self-signed dev CA on first run and persist it
//! under `MOYMOY_CS_TLS_DIR`. Three other `.cs.mnn` backends carried copies of
//! that same code (REIN's `services/rein/src/tls.rs` is the common ancestor).
//! Every one of them produced a certificate no Mochi client has any reason to
//! trust, so the backend came up looking perfectly healthy while Chromium
//! refused every connection to it. There is no fallback path left: with no
//! Hub-issued material, [`server_config`] fails and says which path it looked
//! at.
//!
//! # The debug loop that is gone with it
//!
//! The old self-signed leaf carried `localhost` and `127.0.0.1` in its SAN, so
//! a browser could be pointed straight at `https://localhost:7433` to poke at
//! the wallet without the overlay in the way. **That is no longer possible.**
//! The Hub's `.mnn` CA is constrained (nameConstraints) to the `.mnn`
//! namespace precisely so it can never vouch for a public name, which means a
//! `localhost` SAN cannot be added to a certificate it issues — mixing one in
//! fails validation for the whole chain, so it is not a trade to make, it is a
//! thing that does not work.
//!
//! Two replacements, depending on what is being debugged:
//!
//! * Reach the wallet by its `.mnn` name (`https://moymoy.cs.mnn`) through the
//!   gateway, the way a client does. This exercises the real path, including
//!   the certificate.
//! * Set `MOYMOY_CS_TLS=0` and talk plain HTTP to the listen address. This
//!   skips TLS entirely rather than faking it.

use std::sync::Arc;

use rustls::ServerConfig;

/// Build the rustls [`ServerConfig`] MoyMoy serves `mnn_host` with, from the
/// certificate the Hub provisioned for it.
///
/// A thin call into the SDK: the material's location, its format, and the
/// no-fallback rule are all owned by `mochi_hub_cs_sdk::cs_tls` so that every
/// `.mnn` backend answers those questions identically.
pub fn server_config(mnn_host: &str) -> anyhow::Result<Arc<ServerConfig>> {
    mochi_hub_cs_sdk::cs_tls::server_config(mnn_host).map_err(anyhow::Error::new)
}
