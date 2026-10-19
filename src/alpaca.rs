//! Alpaca paper-trading venue adapter (REST, synchronous).
//!
//! Paper only: the constructor refuses a base URL that is not the paper
//! endpoint. Keys are read into memory by the caller and never printed,
//! logged, or written; the adapter's journal keeps the HTTP status and a
//! SHA-256 of every raw response body, never the body itself.
//!
//! Alpaca reports fills as a running `filled_qty` and `filled_avg_price` on
//! the order rather than as discrete executions, so each poll that observes
//! a larger `filled_qty` yields one `Fill` for the increment, priced so that
//! the running average is preserved. The fill ID is the venue order ID plus
//! the running filled quantity, which makes a re-observed state idempotent.

use crate::{Fill, Order, Side, Venue, VenueEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub unix_ms: u64,
    pub action: String,
    pub client_id: String,
    pub http_status: u16,
    pub response_sha256: String,
    pub venue_status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Tracked {
    venue_id: String,
    acked: bool,
    filled_qty: f64,
    filled_avg_px: f64,
    done: bool,
}

pub struct AlpacaPaper {
    base: String,
    key: String,
    secret: String,
    tracked: BTreeMap<String, Tracked>,
    orders: BTreeMap<String, (String, f64)>,
    pending: Vec<VenueEvent>,
    pub journal: Vec<JournalEntry>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

impl AlpacaPaper {
    pub fn new(base: &str, key: &str, secret: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(base.contains("paper"), "refusing: ALPACA_BASE_URL is not the paper endpoint");
        Ok(Self { base: base.trim_end_matches('/').to_string(), key: key.to_string(), secret: secret.to_string(), tracked: BTreeMap::new(), orders: BTreeMap::new(), pending: Vec::new(), journal: Vec::new() })
    }

    fn req(&self, method: &str, url: &str) -> ureq::Request {
        ureq::request(method, url)
            .set("APCA-API-KEY-ID", &self.key)
            .set("APCA-API-SECRET-KEY", &self.secret)
            .timeout(std::time::Duration::from_secs(20))
    }

    /// Perform a request, journal its status and body hash, return (status, parsed body).
    fn call(&mut self, action: &str, client_id: &str, method: &str, url: &str, body: Option<serde_json::Value>) -> anyhow::Result<(u16, serde_json::Value)> {
        let r = self.req(method, url);
        let resp = match body { Some(b) => r.send_json(b), None => r.call() };
        let (status, text) = match resp {
            Ok(r) => { let s = r.status(); (s, r.into_string()?) }
            Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
            Err(e) => anyhow::bail!("alpaca transport: {e}"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        self.journal.push(JournalEntry { unix_ms: now_ms(), action: action.into(), client_id: client_id.into(), http_status: status, response_sha256: sha256_hex(text.as_bytes()), venue_status: v["status"].as_str().unwrap_or("").to_string() });
        Ok((status, v))
    }

    fn fetch_by_client_id(&mut self, client_id: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        let url = format!("{}/v2/orders:by_client_order_id?client_order_id={client_id}", self.base);
        self.call("get", client_id, "GET", &url, None)
    }

    /// Account snapshot for the record: equity and cash, as strings, nothing else.
    pub fn account(&mut self) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/v2/account", self.base);
        let (status, v) = self.call("account", "", "GET", &url, None)?;
        anyhow::ensure!(status == 200, "account: HTTP {status}");
        Ok(serde_json::json!({ "status": v["status"], "equity": v["equity"], "cash": v["cash"], "buying_power": v["buying_power"] }))
    }

    /// Current position quantity for a symbol as the venue sees it (0 if none).
    pub fn position_qty(&mut self, symbol: &str) -> anyhow::Result<f64> {
        let url = format!("{}/v2/positions/{}", self.base, symbol.replace('/', ""));
        let (status, v) = self.call("position", "", "GET", &url, None)?;
        if status == 404 { return Ok(0.0); }
        anyhow::ensure!(status == 200, "position: HTTP {status}");
        Ok(v["qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0))
    }

    /// Hourly bars, oldest first, completed bars only (the forming hour is dropped).
    pub fn hourly_bars(&mut self, symbol: &str, limit: usize) -> anyhow::Result<Vec<Bar>> {
        let url = format!("https://data.alpaca.markets/v1beta3/crypto/us/bars?symbols={}&timeframe=1Hour&limit={}&sort=desc", symbol.replace('/', "%2F"), limit + 1);
        let (status, v) = self.call("bars", "", "GET", &url, None)?;
        anyhow::ensure!(status == 200, "bars: HTTP {status}");
        let arr = v["bars"][symbol].as_array().cloned().unwrap_or_default();
        let now = now_ms();
        let mut bars: Vec<Bar> = arr.iter().filter_map(|b| {
            let ts_ms = crate::paper::rfc3339_to_ms(b["t"].as_str()?)?;
            Some(Bar { ts_ms, open: b["o"].as_f64()?, high: b["h"].as_f64()?, low: b["l"].as_f64()?, close: b["c"].as_f64()?, volume: b["v"].as_f64().unwrap_or(0.0) })
        }).filter(|b| b.ts_ms + 3_600_000 <= now).collect();
        bars.sort_by_key(|b| b.ts_ms);
        bars.dedup_by_key(|b| b.ts_ms);
        Ok(bars)
    }

    fn observe(&mut self, client_id: &str, v: &serde_json::Value, out: &mut Vec<VenueEvent>) {
        let status = v["status"].as_str().unwrap_or("").to_string();
        let venue_id = v["id"].as_str().unwrap_or("").to_string();
        let filled_qty: f64 = v["filled_qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let filled_avg: f64 = v["filled_avg_price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let t = self.tracked.entry(client_id.to_string()).or_default();
        if t.done { return; }
        if !venue_id.is_empty() { t.venue_id = venue_id; }
        match status.as_str() {
            "rejected" => { t.done = true; out.push(VenueEvent::Rejected { client_id: client_id.into(), reason: v["reject_reason"].as_str().unwrap_or("rejected").into() }); return; }
            "" => return,
            _ => {}
        }
        if !t.acked {
            t.acked = true;
            out.push(VenueEvent::Ack { client_id: client_id.into() });
        }
        if filled_qty > t.filled_qty + 1e-12 {
            let dq = filled_qty - t.filled_qty;
            // Price of the increment that keeps the venue's running average exact.
            let px = if t.filled_qty > 0.0 { (filled_avg * filled_qty - t.filled_avg_px * t.filled_qty) / dq } else { filled_avg };
            let fill = Fill { fill_id: format!("{}:{:.10}", t.venue_id, filled_qty), qty: (dq * 1e10).round() / 1e10, px };
            t.filled_qty = filled_qty;
            t.filled_avg_px = filled_avg;
            out.push(VenueEvent::Fill { client_id: client_id.into(), fill });
        }
        match status.as_str() {
            "filled" => t.done = true,
            "canceled" | "expired" | "done_for_day" | "replaced" => { t.done = true; out.push(VenueEvent::Cancelled { client_id: client_id.into() }); }
            _ => {}
        }
    }

    pub fn all_done(&self) -> bool {
        self.tracked.values().all(|t| t.done)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bar {
    pub ts_ms: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Venue for AlpacaPaper {
    fn submit(&mut self, order: &Order) -> anyhow::Result<()> {
        let id = order.client_id.clone();
        self.orders.insert(id.clone(), (order.intent.symbol.clone(), order.intent.qty));
        let body = serde_json::json!({
            "symbol": order.intent.symbol,
            "qty": format!("{:.8}", order.intent.qty),
            "side": match order.intent.side { Side::Buy => "buy", Side::Sell => "sell" },
            "type": "market",
            "time_in_force": "gtc",
            "client_order_id": id,
        });
        let url = format!("{}/v2/orders", self.base);
        let (status, v) = self.call("submit", &id, "POST", &url, Some(body))?;
        let mut evs = Vec::new();
        match status {
            200 | 201 => self.observe(&id, &v, &mut evs),
            422 if v["message"].as_str().map_or(false, |m| m.contains("client_order_id")) => {
                // Already known to the venue: adopt its state rather than placing a second order.
                let (_, existing) = self.fetch_by_client_id(&id)?;
                self.observe(&id, &existing, &mut evs);
            }
            s => {
                let t = self.tracked.entry(id.clone()).or_default();
                t.done = true;
                evs.push(VenueEvent::Rejected { client_id: id.clone(), reason: format!("HTTP {s}: {}", v["message"].as_str().unwrap_or("")) });
            }
        }
        self.pending.extend(evs);
        Ok(())
    }
    fn cancel(&mut self, client_id: &str) -> anyhow::Result<()> {
        let venue_id = self.tracked.get(client_id).map(|t| t.venue_id.clone()).unwrap_or_default();
        anyhow::ensure!(!venue_id.is_empty(), "cancel: no venue id for {client_id}");
        let url = format!("{}/v2/orders/{venue_id}", self.base);
        let (status, _) = self.call("cancel", client_id, "DELETE", &url, None)?;
        anyhow::ensure!(status == 204 || status == 200, "cancel: HTTP {status}");
        Ok(())
    }
    fn poll(&mut self) -> Vec<VenueEvent> {
        let mut out = std::mem::take(&mut self.pending);
        let ids: Vec<String> = self.tracked.iter().filter(|(_, t)| !t.done).map(|(k, _)| k.clone()).collect();
        for id in ids {
            match self.fetch_by_client_id(&id) {
                Ok((200, v)) => self.observe(&id, &v, &mut out),
                Ok(_) | Err(_) => {}
            }
        }
        out
    }
}
