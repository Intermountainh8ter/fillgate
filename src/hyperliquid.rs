//! Hyperliquid testnet venue adapter.
//!
//! Testnet only: the constructor refuses a base URL that is not the testnet.
//! Orders are signed with the wallet key as Hyperliquid "L1 actions": the
//! action is msgpack-encoded with string keys in wire order, hashed with the
//! nonce and the vault marker, and that hash becomes the `connectionId` of a
//! phantom `Agent` signed as EIP-712 typed data (domain `Exchange`, version
//! `1`, chain ID 1337). A market order is a limit order at the mark price
//! moved by the slippage allowance with time-in-force IOC, which is how the
//! venue's own SDK does it. The fillgate client ID (16 bytes, hex) is sent
//! as the order's `cloid`, so the venue's own idempotency applies.
//!
//! Keys are read into memory by the caller and never printed, logged or
//! written. The journal keeps HTTP status and a SHA-256 of every raw
//! response body, never the body itself.

use crate::alpaca::JournalEntry;
use crate::{Fill, Order, Side, Venue, VenueEvent};
use alloy_primitives::{keccak256, Address, B256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{eip712_domain, sol, SolStruct};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

sol! {
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// Wire-order structs: msgpack key order must match the venue's reference SDK exactly.
#[derive(Serialize)]
struct WireLimit { tif: String }
#[derive(Serialize)]
struct WireOrderType { limit: WireLimit }
#[derive(Serialize)]
struct WireOrder {
    a: u32,
    b: bool,
    p: String,
    s: String,
    r: bool,
    t: WireOrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<String>,
}
#[derive(Serialize)]
struct WireOrderAction {
    #[serde(rename = "type")]
    kind: String,
    orders: Vec<WireOrder>,
    grouping: String,
}
#[derive(Serialize)]
struct WireCancelByCloid { asset: u32, cloid: String }
#[derive(Serialize)]
struct WireCancelAction {
    #[serde(rename = "type")]
    kind: String,
    cancels: Vec<WireCancelByCloid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sig { pub r: String, pub s: String, pub v: u64 }

/// Hash of an L1 action as the venue computes it: msgpack(action) ‖ nonce (u64 big-endian) ‖ 0x00, or ‖ 0x01 ‖ vault address.
pub fn action_hash(action_msgpack: &[u8], nonce: u64, vault: Option<Address>) -> B256 {
    let mut data = Vec::with_capacity(action_msgpack.len() + 29);
    data.extend_from_slice(action_msgpack);
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault {
        None => data.push(0u8),
        Some(a) => { data.push(1u8); data.extend_from_slice(a.as_slice()); }
    }
    keccak256(&data)
}

/// Sign an L1 action. `is_mainnet` selects the phantom agent source ("a" mainnet, "b" testnet).
pub fn sign_l1_action(signer: &PrivateKeySigner, action_msgpack: &[u8], nonce: u64, vault: Option<Address>, is_mainnet: bool) -> anyhow::Result<Sig> {
    let agent = Agent { source: if is_mainnet { "a".into() } else { "b".into() }, connectionId: action_hash(action_msgpack, nonce, vault) };
    let domain = eip712_domain! { name: "Exchange", version: "1", chain_id: 1337, verifying_contract: Address::ZERO, };
    let digest = agent.eip712_signing_hash(&domain);
    let sig = signer.sign_hash_sync(&digest)?;
    Ok(Sig { r: format!("0x{:064x}", sig.r()), s: format!("0x{:064x}", sig.s()), v: if sig.v() { 28 } else { 27 } })
}

pub fn msgpack_named<T: Serialize>(v: &T) -> anyhow::Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(v)?)
}

#[derive(Debug, Clone, Default)]
struct Tracked { asset: u32, done: bool, acked: bool }

pub struct Hyperliquid {
    base: String,
    signer: PrivateKeySigner,
    address: Address,
    is_mainnet: bool,
    /// Asset index and size decimals by coin, from the `meta` info call.
    assets: BTreeMap<String, (u32, u32)>,
    tracked: BTreeMap<String, Tracked>,
    pending: Vec<VenueEvent>,
    pub slippage_bps: f64,
    pub journal: Vec<JournalEntry>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

impl Hyperliquid {
    /// `key_hex` is the wallet private key (with or without 0x). Testnet only.
    pub fn new(base: &str, key_hex: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(base.contains("testnet"), "refusing: HL_BASE_URL is not the testnet");
        let signer: PrivateKeySigner = key_hex.trim().trim_start_matches("0x").parse().map_err(|_| anyhow::anyhow!("HL private key is not a 32-byte hex string"))?;
        let address = signer.address();
        Ok(Self { base: base.trim_end_matches('/').to_string(), signer, address, is_mainnet: false, assets: BTreeMap::new(), tracked: BTreeMap::new(), pending: Vec::new(), slippage_bps: 500.0, journal: Vec::new() })
    }

    pub fn address(&self) -> Address { self.address }

    fn post(&mut self, action: &str, client_id: &str, path: &str, body: serde_json::Value) -> anyhow::Result<(u16, serde_json::Value)> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::post(&url).timeout(std::time::Duration::from_secs(20)).send_json(body);
        let (status, text) = match resp {
            Ok(r) => { let s = r.status(); (s, r.into_string()?) }
            Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
            Err(e) => anyhow::bail!("hyperliquid transport: {e}"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone()));
        self.journal.push(JournalEntry { unix_ms: now_ms(), action: action.into(), client_id: client_id.into(), http_status: status, response_sha256: sha256_hex(text.as_bytes()), venue_status: v["status"].as_str().unwrap_or("").to_string() });
        Ok((status, v))
    }

    fn info(&mut self, action: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let (status, v) = self.post(action, "", "/info", body)?;
        anyhow::ensure!(status == 200, "info {action}: HTTP {status}");
        Ok(v)
    }

    fn load_meta(&mut self) -> anyhow::Result<()> {
        if !self.assets.is_empty() { return Ok(()); }
        let v = self.info("meta", serde_json::json!({"type": "meta"}))?;
        for (i, a) in v["universe"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            if let Some(name) = a["name"].as_str() {
                self.assets.insert(name.to_string(), (i as u32, a["szDecimals"].as_u64().unwrap_or(0) as u32));
            }
        }
        anyhow::ensure!(!self.assets.is_empty(), "meta returned no universe");
        Ok(())
    }

    fn coin(symbol: &str) -> &str {
        symbol.split(['/', '-']).next().unwrap_or(symbol)
    }

    fn mark_px(&mut self, coin: &str) -> anyhow::Result<f64> {
        let v = self.info("allMids", serde_json::json!({"type": "allMids"}))?;
        v[coin].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("no mid for {coin}"))
    }

    /// Account summary for `check`: account value and withdrawable, nothing else.
    pub fn account(&mut self) -> anyhow::Result<serde_json::Value> {
        let addr = format!("{:?}", self.address);
        let v = self.info("clearinghouseState", serde_json::json!({"type": "clearinghouseState", "user": addr}))?;
        Ok(serde_json::json!({ "account_value": v["marginSummary"]["accountValue"], "withdrawable": v["withdrawable"], "n_positions": v["assetPositions"].as_array().map(|a| a.len()).unwrap_or(0) }))
    }

    /// Hyperliquid's significant-figure rule: at most 5 significant figures, and no more decimals than 6 - szDecimals (perps).
    fn fmt_px(px: f64, sz_decimals: u32) -> String {
        let max_dec = 6u32.saturating_sub(sz_decimals);
        let mut s = format!("{:.4e}", px);
        // Round to 5 significant figures then re-render as a plain decimal with at most max_dec places.
        let v: f64 = s.parse().unwrap_or(px);
        s = format!("{:.*}", max_dec as usize, v);
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        if s.is_empty() { "0".into() } else { s }
    }

    fn fmt_sz(sz: f64, sz_decimals: u32) -> String {
        let s = format!("{:.*}", sz_decimals as usize, sz);
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        if s.is_empty() { "0".into() } else { s }
    }

    fn signed_exchange(&mut self, action_name: &str, client_id: &str, action_bytes: Vec<u8>, action_json: serde_json::Value) -> anyhow::Result<(u16, serde_json::Value)> {
        let nonce = now_ms();
        let sig = sign_l1_action(&self.signer, &action_bytes, nonce, None, self.is_mainnet)?;
        let body = serde_json::json!({ "action": action_json, "nonce": nonce, "signature": sig, "vaultAddress": serde_json::Value::Null });
        self.post(action_name, client_id, "/exchange", body)
    }
}

impl Venue for Hyperliquid {
    fn submit(&mut self, order: &Order) -> anyhow::Result<()> {
        self.load_meta()?;
        let id = order.client_id.clone();
        let coin = Self::coin(&order.intent.symbol).to_string();
        let (asset, sz_dec) = *self.assets.get(&coin).ok_or_else(|| anyhow::anyhow!("unknown coin {coin}"))?;
        let is_buy = matches!(order.intent.side, Side::Buy);
        let px = match order.intent.limit_px {
            Some(p) => p,
            None => { let m = self.mark_px(&coin)?; if is_buy { m * (1.0 + self.slippage_bps / 10_000.0) } else { m * (1.0 - self.slippage_bps / 10_000.0) } }
        };
        let cloid = format!("0x{:0>32}", id);
        let wire = WireOrderAction { kind: "order".into(), orders: vec![WireOrder { a: asset, b: is_buy, p: Self::fmt_px(px, sz_dec), s: Self::fmt_sz(order.intent.qty, sz_dec), r: false, t: WireOrderType { limit: WireLimit { tif: "Ioc".into() } }, c: Some(cloid.clone()) }], grouping: "na".into() };
        let bytes = msgpack_named(&wire)?;
        let json = serde_json::to_value(&wire)?;
        self.tracked.insert(id.clone(), Tracked { asset, ..Default::default() });
        let (status, v) = self.signed_exchange("submit", &id, bytes, json)?;
        let t = self.tracked.get_mut(&id).unwrap();
        if status != 200 || v["status"] != "ok" {
            t.done = true;
            self.pending.push(VenueEvent::Rejected { client_id: id, reason: format!("HTTP {status}: {}", v["response"].as_str().or(v.as_str()).unwrap_or("")) });
            return Ok(());
        }
        let st = &v["response"]["data"]["statuses"][0];
        if let Some(e) = st["error"].as_str() {
            t.done = true;
            self.pending.push(VenueEvent::Rejected { client_id: id, reason: e.to_string() });
            return Ok(());
        }
        t.acked = true;
        self.pending.push(VenueEvent::Ack { client_id: id.clone() });
        if let Some(f) = st.get("filled") {
            // Immediate fill on an IOC: the resting-order poll will pick up the fill IDs; record the total now by venue oid.
            let qty: f64 = f["totalSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let px: f64 = f["avgPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let oid = f["oid"].as_u64().unwrap_or(0);
            if qty > 0.0 {
                self.pending.push(VenueEvent::Fill { client_id: id.clone(), fill: Fill { fill_id: format!("oid{oid}:{qty:.10}"), qty, px } });
            }
            t.done = true;
            if (qty - order.intent.qty).abs() > 1e-9 {
                self.pending.push(VenueEvent::Cancelled { client_id: id });
            }
        } else if st.get("resting").is_none() {
            // IOC that neither filled nor rested: cancelled with nothing done.
            t.done = true;
            self.pending.push(VenueEvent::Cancelled { client_id: id });
        }
        Ok(())
    }

    fn cancel(&mut self, client_id: &str) -> anyhow::Result<()> {
        let asset = self.tracked.get(client_id).map(|t| t.asset).ok_or_else(|| anyhow::anyhow!("cancel: unknown order {client_id}"))?;
        let wire = WireCancelAction { kind: "cancelByCloid".into(), cancels: vec![WireCancelByCloid { asset, cloid: format!("0x{:0>32}", client_id) }] };
        let bytes = msgpack_named(&wire)?;
        let json = serde_json::to_value(&wire)?;
        let (status, v) = self.signed_exchange("cancel", client_id, bytes, json)?;
        anyhow::ensure!(status == 200 && v["status"] == "ok", "cancel: HTTP {status} {}", v);
        Ok(())
    }

    fn poll(&mut self) -> Vec<VenueEvent> {
        let mut out = std::mem::take(&mut self.pending);
        let open: Vec<String> = self.tracked.iter().filter(|(_, t)| !t.done).map(|(k, _)| k.clone()).collect();
        if open.is_empty() { return out; }
        let addr = format!("{:?}", self.address);
        for id in open {
            let cloid = format!("0x{:0>32}", id);
            let Ok(v) = self.info("orderStatus", serde_json::json!({"type": "orderStatus", "user": addr, "oid": cloid})) else { continue };
            let status = v["order"]["status"].as_str().unwrap_or("").to_string();
            let t = self.tracked.get_mut(&id).unwrap();
            match status.as_str() {
                "filled" | "canceled" | "rejected" | "marginCanceled" => {
                    t.done = true;
                    if status == "rejected" { out.push(VenueEvent::Rejected { client_id: id.clone(), reason: status.clone() }); }
                    else if status != "filled" { out.push(VenueEvent::Cancelled { client_id: id.clone() }); }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> PrivateKeySigner {
        "0123456789012345678901234567890123456789012345678901234567890123".parse().unwrap()
    }

    fn reference_order() -> WireOrderAction {
        WireOrderAction { kind: "order".into(), orders: vec![WireOrder { a: 1, b: true, p: "100".into(), s: "100".into(), r: false, t: WireOrderType { limit: WireLimit { tif: "Gtc".into() } }, c: None }], grouping: "na".into() }
    }

    #[test]
    fn msgpack_is_named_and_in_wire_order() {
        let b = msgpack_named(&reference_order()).unwrap();
        // fixmap(3) "type" "order" "orders" [fixmap(6) ...] "grouping" "na"
        assert_eq!(b[0], 0x83);
        assert_eq!(&b[1..6], b"\xa4type");
        assert_eq!(&b[6..12], b"\xa5order");
        assert_eq!(&b[12..19], b"\xa6orders");
    }

    /// Reference vectors from the venue's Python SDK test-suite (test_l1_action_signing_order_matches),
    /// private key 0x0123...0123, nonce 0, the order above.
    #[test]
    fn l1_action_signature_matches_reference_sdk() {
        let b = msgpack_named(&reference_order()).unwrap();
        let s = test_signer();
        let mainnet = sign_l1_action(&s, &b, 0, None, true).unwrap();
        assert_eq!(mainnet.r, "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e");
        assert_eq!(mainnet.s, "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e");
        assert_eq!(mainnet.v, 28);
        let testnet = sign_l1_action(&s, &b, 0, None, false).unwrap();
        assert_eq!(testnet.r, "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54");
        assert_eq!(testnet.s, "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5");
        assert_eq!(testnet.v, 27);
    }

    #[test]
    fn price_and_size_formatting_follow_the_venue_rules() {
        assert_eq!(Hyperliquid::fmt_sz(0.01, 5), "0.01");
        assert_eq!(Hyperliquid::fmt_sz(1.0, 3), "1");
        assert_eq!(Hyperliquid::fmt_px(112345.678, 5), "112350");
        assert_eq!(Hyperliquid::fmt_px(1.234567, 1), "1.2346");
    }

    #[test]
    fn testnet_only() {
        assert!(Hyperliquid::new("https://api.hyperliquid.xyz", "01".repeat(32).as_str()).is_err());
        assert!(Hyperliquid::new("https://api.hyperliquid-testnet.xyz", "01".repeat(32).as_str()).is_ok());
    }
}
