//! MochiOS command-bus client (mc-sdk) — bidirectional link to the in-world mod.
//!
//! This is the OPTIONAL emerald-charge path. The wallet (balance/send/pay/history)
//! works entirely without it; when the command bus is not configured the backend
//! runs "wallet-only" (`can_charge = false`) and the charge/inventory endpoints
//! degrade gracefully — mirroring PiggleShop's catalog-only degrade.
//!
//! TODO(task#5): real wiring. The bus will:
//!   - `McSdk::connect` with `cs_hosts = ["moymoy"]` so the mod's reply
//!     (`reply.reply("moymoy", …)`, routed by the sidecar to `moymoy.cs.mnn`)
//!     lands on our `run_inbound` receiver;
//!   - `reliable_send("moymoy.<UUID>.minecraft.auto.mnn", "moymoy", payload)` to
//!     ask the mod to consume emeralds (auto-routed to the player's live server);
//!   - `run_inbound` to receive the mod's settlement acks → charge.rs.
//! Connection requires `MOCHI_MC_CERT_DIR` (chain.pem/leaf.key.pem/ca.cert.pem),
//! minted by `mochi-mc-ca issue --mcserver-id moymoy`.

/// A connected command bus. Cloneable handle (task#5: wraps an `Arc<McSdk>` +
/// reconnect state). For now it carries nothing — `connect` returns `None` until
/// task#5 wires the cert-gated mc-sdk connection.
#[derive(Clone)]
pub struct CommandBus {
    _private: (),
}

impl CommandBus {
    /// Connect to the Hub command bus when `MOCHI_MC_CERT_DIR` is configured.
    /// Returns `Ok(None)` (degraded wallet-only mode) when no cert dir is set —
    /// never fails the boot for a missing optional integration.
    pub async fn connect() -> anyhow::Result<Option<CommandBus>> {
        match std::env::var("MOCHI_MC_CERT_DIR")
            .ok()
            .filter(|s| !s.is_empty())
        {
            None => {
                tracing::warn!(
                    "MOCHI_MC_CERT_DIR unset — running WALLET-ONLY (emerald charge disabled). \
                     Mint a cert: mochi-mc-ca issue --mcserver-id moymoy --out <dir>"
                );
                Ok(None)
            }
            Some(_dir) => {
                // TODO(task#5): load_client_identity + McSdk::connect(cs_hosts=["moymoy"])
                // + spawn run_inbound. Until then, stay degraded but say so loudly.
                tracing::warn!(
                    "MOCHI_MC_CERT_DIR is set but the command bus is not wired yet \
                     (task#5) — running WALLET-ONLY"
                );
                Ok(None)
            }
        }
    }
}
