//! MochiOS command-bus client (mc-sdk) — bidirectional link to the in-world mod.
//!
//! This is the OPTIONAL emerald-charge path. The wallet (balance/send/pay/history)
//! works entirely without it; when no MC cert is configured the backend runs
//! "wallet-only" (`can_charge = false`) and charge/inventory degrade gracefully
//! (mirroring PiggleShop's catalog-only degrade).
//!
//! Single `McSdk` connection, two directions (DEV.md §7.3):
//!   - OUTBOUND `reliable_send("moymoy.<UUID>.minecraft.auto.mnn", "moymoy", …)` —
//!     ask the mod to consume emeralds / report inventory, auto-routed to the
//!     player's live server.
//!   - INBOUND `run_inbound` — because we connect with `cs_hosts = ["moymoy"]`,
//!     the mod's reply (`reply.reply("moymoy", …)`, which the mc-connector sidecar
//!     routes as `moymoy.cs.mnn`) lands here. A charge ack (`op_id`) settles the
//!     `emerald_ops` ledger; an inventory reply (`req_id`) wakes a pending query.
//!
//! A supervisor task owns connect → run_inbound → reconnect, publishing the live
//! `McSdk` into a slot that `send_*` reads (None while reconnecting, so a send
//! fails cleanly and reconciliation retries later).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mochi_hub_mc_pki::load_client_identity;
use mochi_hub_mc_sdk::{McSdk, McSdkConfig, SdkError};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use uuid::Uuid;

use crate::charge;
use crate::db::Pool;

/// Result of asking the mod to do something over the bus.
pub enum SendOutcome {
    /// Reliably relayed toward the player's live server (the mod settles via ack).
    Sent,
    /// The player is not in the presence directory (Hub bounced 404).
    PlayerOffline,
    /// The bus is currently disconnected (reconnecting).
    BusDown,
    /// Transport/SDK error.
    Error(String),
}

/// Connection materials kept so the supervisor can rebuild the config and
/// reconnect after an idle drop (mc-sdk types are not `Clone`, so we hold the
/// raw cert material and re-derive `McSdkConfig` each connect).
struct ConnMaterials {
    hub_addr: SocketAddr,
    server_name: String,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    ca_roots: RootCertStore,
}

impl ConnMaterials {
    fn config(&self) -> McSdkConfig {
        McSdkConfig {
            hub_addr: self.hub_addr,
            server_name: self.server_name.clone(),
            client_cert_chain: self.chain.clone(),
            client_key: self.key.clone_key(),
            ca_roots: self.ca_roots.clone(),
            // node_id keys the reverse tunnel; routing to us is by the cs claim,
            // so a fresh id per connect is fine (matches the grant-sender pattern).
            node_id: Uuid::new_v4(),
            // Claim cs host "moymoy" so the mod's reply (routed as moymoy.cs.mnn)
            // is delivered to THIS connection's run_inbound.
            cs_hosts: vec!["moymoy".to_string()],
        }
    }
}

/// A connected command bus (a cheap handle; the connection lives in the
/// supervisor task and is published through `slot`).
#[derive(Clone)]
pub struct CommandBus {
    slot: Arc<RwLock<Option<Arc<McSdk>>>>,
    pending_inv: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl CommandBus {
    /// Connect to the Hub command bus when `MOCHI_MC_CERT_DIR` is configured and
    /// holds the PEMs. Returns `Ok(None)` (degraded wallet-only) when unset or
    /// missing — never fails the boot for a missing optional integration.
    pub async fn connect(pool: Pool) -> anyhow::Result<Option<CommandBus>> {
        let cert_dir = match std::env::var("MOCHI_MC_CERT_DIR")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(d) => PathBuf::from(d),
            None => {
                tracing::warn!(
                    "MOCHI_MC_CERT_DIR unset — running WALLET-ONLY (emerald charge disabled). \
                     Mint a cert: mochi-mc-ca issue --mcserver-id moymoy --out <dir>"
                );
                return Ok(None);
            }
        };
        let chain_p = cert_dir.join("chain.pem");
        let key_p = cert_dir.join("leaf.key.pem");
        let ca_p = cert_dir.join("ca.cert.pem");
        if !chain_p.exists() || !key_p.exists() || !ca_p.exists() {
            tracing::warn!(dir = %cert_dir.display(),
                "cert dir missing chain.pem/leaf.key.pem/ca.cert.pem — WALLET-ONLY mode");
            return Ok(None);
        }

        let (chain, key, ca_roots) = load_client_identity(&chain_p, &key_p, &ca_p)?;
        let hub_addr: SocketAddr = std::env::var("MOCHI_MC_HUB_QUIC")
            .unwrap_or_else(|_| "127.0.0.1:7421".to_string())
            .parse()
            .map_err(|e| anyhow::anyhow!("MOCHI_MC_HUB_QUIC parse: {e}"))?;
        let server_name =
            std::env::var("MOCHI_MC_SERVER_NAME").unwrap_or_else(|_| "localhost".to_string());

        let materials = Arc::new(ConnMaterials {
            hub_addr,
            server_name,
            chain,
            key,
            ca_roots,
        });
        let slot: Arc<RwLock<Option<Arc<McSdk>>>> = Arc::new(RwLock::new(None));
        let pending_inv: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(supervise(
            pool,
            materials,
            slot.clone(),
            pending_inv.clone(),
        ));

        tracing::info!(%hub_addr, "command bus enabled (app_id=moymoy, cs claim=moymoy)");
        Ok(Some(CommandBus { slot, pending_inv }))
    }

    async fn live(&self) -> Option<Arc<McSdk>> {
        self.slot.read().await.clone()
    }

    /// Ask the mod to consume `amount` emeralds for `uuid` (auto-routed). The mod
    /// settles asynchronously via an ack to `op_id`.
    pub async fn send_charge(
        &self,
        uuid: &Uuid,
        op_id: &str,
        idem_key: &str,
        amount: i64,
    ) -> SendOutcome {
        let dst = format!("moymoy.{uuid}.minecraft.auto.mnn");
        let payload = serde_json::json!({
            "op_id": op_id,
            "idem_key": idem_key,
            "verb": "emerald.charge",
            "target_uuid": uuid.to_string(),
            "amount": amount,
        });
        match self.live().await {
            None => SendOutcome::BusDown,
            Some(sdk) => match sdk
                .reliable_send(&dst, "moymoy", payload.to_string().as_bytes())
                .await
            {
                Ok(()) => SendOutcome::Sent,
                Err(SdkError::NotDelivered { status: 404, .. }) => SendOutcome::PlayerOffline,
                Err(e) => SendOutcome::Error(e.to_string()),
            },
        }
    }

    /// Round-trip the player's chargeable inventory (emeralds, blocks) via the
    /// mod. `None` on timeout / offline / bus-down.
    pub async fn query_inventory(&self, uuid: &Uuid) -> Option<(i64, i64)> {
        let req_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending_inv.lock().await.insert(req_id.clone(), tx);

        let dst = format!("moymoy.{uuid}.minecraft.auto.mnn");
        let payload = serde_json::json!({
            "req_id": req_id,
            "verb": "inventory.query",
            "target_uuid": uuid.to_string(),
        });
        let sdk = match self.live().await {
            Some(s) => s,
            None => {
                self.pending_inv.lock().await.remove(&req_id);
                return None;
            }
        };
        if sdk
            .reliable_send(&dst, "moymoy", payload.to_string().as_bytes())
            .await
            .is_err()
        {
            self.pending_inv.lock().await.remove(&req_id);
            return None;
        }
        match tokio::time::timeout(Duration::from_secs(3), rx).await {
            Ok(Ok(v)) => {
                let e = v.get("emeralds").and_then(Value::as_i64)?;
                let b = v.get("blocks").and_then(Value::as_i64)?;
                Some((e, b))
            }
            _ => {
                self.pending_inv.lock().await.remove(&req_id);
                None
            }
        }
    }
}

/// Resolve a Minecraft username (MCID) to its canonical UUID via the Hub's
/// `<title>.auto.mnn` directory (DEV.md §7.3.8), reached through the IPvM gateway
/// as a forward proxy. `None` ⇒ not currently online / unknown. (Adapted from
/// PiggleShop2/server/piggleshop-cs/src/grant.rs.)
pub async fn resolve_mcid(mcid: &str) -> Option<Uuid> {
    let gateway = std::env::var("MOCHI_IPVM_GATEWAY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1:7411".to_string());
    let proxy = reqwest::Proxy::http(format!("http://{gateway}")).ok()?;
    let client = reqwest::Client::builder().proxy(proxy).build().ok()?;
    let url = format!("http://minecraft.auto.mnn/v1/resolve/{mcid}");
    let resp = client.get(&url).send().await.ok()?;
    let v: Value = resp.json().await.ok()?;
    v.get("player")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Supervisor: connect → run_inbound (blocks until disconnect) → reconnect. The
/// live `McSdk` is published in `slot` for `send_*`; inbound frames are dispatched
/// (charge ack → ledger settle; inventory reply → wake the pending query).
async fn supervise(
    pool: Pool,
    materials: Arc<ConnMaterials>,
    slot: Arc<RwLock<Option<Arc<McSdk>>>>,
    pending_inv: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
) {
    loop {
        match McSdk::connect(materials.config()).await {
            Ok(sdk) => {
                let sdk = Arc::new(sdk);
                *slot.write().await = Some(sdk.clone());
                tracing::info!("command bus connected (moymoy)");

                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let pool_s = pool.clone();
                let pend_s = pending_inv.clone();
                let settler = tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        dispatch_inbound(&pool_s, &pend_s, data).await;
                    }
                });

                // run_inbound blocks until the connection closes.
                let _ = sdk
                    .run_inbound(move |_hdr, data| {
                        let _ = tx.send(data);
                    })
                    .await;

                *slot.write().await = None;
                settler.abort();
                tracing::warn!("command bus disconnected; reconnecting in 5s");
            }
            Err(e) => {
                tracing::warn!(error = %e, "command bus connect failed; retry in 5s");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Route one inbound frame: an inventory reply (`req_id`) wakes its pending query;
/// a charge ack (`op_id`) settles the ledger (off the async thread via
/// `spawn_blocking`).
async fn dispatch_inbound(
    pool: &Pool,
    pending_inv: &Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    data: Vec<u8>,
) {
    let v: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "command bus: malformed inbound frame; dropping");
            return;
        }
    };

    if let Some(req_id) = v.get("req_id").and_then(Value::as_str) {
        if let Some(tx) = pending_inv.lock().await.remove(req_id) {
            let _ = tx.send(v);
        }
        return;
    }

    if v.get("op_id").is_some() {
        let pool = pool.clone();
        let _ = tokio::task::spawn_blocking(move || {
            match pool.get() {
                Ok(mut conn) => {
                    if let Err(e) = charge::settle_ack(&mut conn, &v) {
                        tracing::error!(error = %e, "command bus: charge settle failed");
                    }
                }
                Err(e) => tracing::error!(error = %e, "command bus: pool get for settle failed"),
            }
        })
        .await;
        return;
    }

    tracing::warn!(frame = %v, "command bus: inbound frame with neither req_id nor op_id; dropping");
}
