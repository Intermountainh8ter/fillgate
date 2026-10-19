//! fillgate: the gate in front of every order.
//!
//! An order passes through, in this order: an idempotent client ID (so a
//! retry can never place a second order), the risk gate (position, notional,
//! daily loss), the kill switch (a file on disk or a drawdown breach), and
//! only then a venue. Every state transition is validated; an illegal one is
//! an error, not a warning. The simulated exchange exists to break the
//! engine on purpose: latency, partial fills, duplicate acknowledgements,
//! dropped acknowledgements, and disconnects, all from a seeded generator so
//! a failure reproduces.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod alpaca;
pub mod paper;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// What a strategy wants. The client ID is derived from it, never invented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub strategy: String,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub limit_px: Option<f64>,
    /// The bar or event the decision was made on; part of the ID so the same
    /// decision resubmitted maps to the same order.
    pub decision_ts_ms: i64,
}

/// Deterministic client order ID: the same intent always yields the same ID.
pub fn client_id(i: &Intent) -> String {
    use sha2::Digest;
    let key = format!("{}|{}|{:?}|{:.10}|{:?}|{}", i.strategy, i.symbol, i.side, i.qty, i.limit_px.map(|p| format!("{p:.10}")), i.decision_ts_ms);
    hex::encode(&sha2::Sha256::digest(key.as_bytes())[..16])
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum State {
    New,
    Submitted,
    Acked,
    PartiallyFilled { filled: f64 },
    Filled { filled: f64, avg_px: f64 },
    Cancelled { filled: f64 },
    Rejected { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub client_id: String,
    pub intent: Intent,
    pub state: State,
    pub fills: Vec<Fill>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    /// Venue fill ID; duplicates by this ID are ignored.
    pub fill_id: String,
    pub qty: f64,
    pub px: f64,
}

#[derive(Debug)]
pub enum TransitionError {
    Illegal { from: State, event: String },
    Overfill { filled: f64, qty: f64 },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::Illegal { from, event } => write!(f, "illegal transition from {from:?} on {event}"),
            TransitionError::Overfill { filled, qty } => write!(f, "overfill: {filled} > {qty}"),
        }
    }
}
impl std::error::Error for TransitionError {}

impl Order {
    pub fn new(intent: Intent) -> Self {
        Self { client_id: client_id(&intent), intent, state: State::New, fills: Vec::new() }
    }
    fn filled_qty(&self) -> f64 {
        self.fills.iter().map(|f| f.qty).sum()
    }
    pub fn on_submitted(&mut self) -> Result<(), TransitionError> {
        match self.state {
            State::New => { self.state = State::Submitted; Ok(()) }
            ref s => Err(TransitionError::Illegal { from: s.clone(), event: "submitted".into() }),
        }
    }
    /// A duplicate acknowledgement is a no-op, not an error: venues resend them.
    pub fn on_acked(&mut self) -> Result<(), TransitionError> {
        match self.state {
            State::Submitted => { self.state = State::Acked; Ok(()) }
            State::Acked | State::PartiallyFilled { .. } => Ok(()),
            ref s => Err(TransitionError::Illegal { from: s.clone(), event: "acked".into() }),
        }
    }
    /// A fill with an already-seen fill ID is ignored; an overfill is an error.
    pub fn on_fill(&mut self, fill: Fill) -> Result<(), TransitionError> {
        match self.state {
            State::Acked | State::PartiallyFilled { .. } | State::Submitted => {}
            ref s => return Err(TransitionError::Illegal { from: s.clone(), event: format!("fill {}", fill.fill_id) }),
        }
        if self.fills.iter().any(|f| f.fill_id == fill.fill_id) {
            return Ok(());
        }
        let total = self.filled_qty() + fill.qty;
        if total > self.intent.qty + 1e-9 {
            return Err(TransitionError::Overfill { filled: total, qty: self.intent.qty });
        }
        self.fills.push(fill);
        let filled = self.filled_qty();
        if (filled - self.intent.qty).abs() < 1e-9 {
            let notional: f64 = self.fills.iter().map(|f| f.qty * f.px).sum();
            self.state = State::Filled { filled, avg_px: notional / filled };
        } else {
            self.state = State::PartiallyFilled { filled };
        }
        Ok(())
    }
    pub fn on_cancelled(&mut self) -> Result<(), TransitionError> {
        match self.state {
            State::Submitted | State::Acked | State::PartiallyFilled { .. } => { self.state = State::Cancelled { filled: self.filled_qty() }; Ok(()) }
            ref s => Err(TransitionError::Illegal { from: s.clone(), event: "cancelled".into() }),
        }
    }
    pub fn on_rejected(&mut self, reason: &str) -> Result<(), TransitionError> {
        match self.state {
            State::New | State::Submitted => { self.state = State::Rejected { reason: reason.into() }; Ok(()) }
            ref s => Err(TransitionError::Illegal { from: s.clone(), event: "rejected".into() }),
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, State::Filled { .. } | State::Cancelled { .. } | State::Rejected { .. })
    }
}

/// Limits fixed before trading starts. All in the account's quote currency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_position_qty: f64,
    pub max_order_notional: f64,
    pub max_daily_loss: f64,
    /// Equity drawdown from the session high that trips the kill switch.
    pub max_drawdown: f64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Ledger {
    pub positions: BTreeMap<String, f64>,
    pub realized_pnl: f64,
    pub equity_high: f64,
    pub equity: f64,
    /// Average entry price per symbol for realized P&L.
    pub avg_px: BTreeMap<String, f64>,
}

impl Ledger {
    pub fn apply_fill(&mut self, symbol: &str, side: Side, qty: f64, px: f64) {
        let pos = self.positions.entry(symbol.to_string()).or_insert(0.0);
        let avg = self.avg_px.entry(symbol.to_string()).or_insert(0.0);
        let signed = side.sign() * qty;
        if (*pos).signum() == signed.signum() || *pos == 0.0 {
            // Adding to or opening a position: update average entry.
            let new_pos = *pos + signed;
            *avg = (avg.abs() * pos.abs() + px * qty) / new_pos.abs();
            *pos = new_pos;
        } else {
            // Reducing or flipping: realize P&L on the closed part.
            let closing = qty.min(pos.abs());
            self.realized_pnl += (px - *avg) * closing * pos.signum();
            let new_pos = *pos + signed;
            if new_pos.signum() != pos.signum() && new_pos != 0.0 {
                *avg = px;
            }
            *pos = new_pos;
            if *pos == 0.0 {
                *avg = 0.0;
            }
        }
        self.equity = self.realized_pnl;
        self.equity_high = self.equity_high.max(self.equity);
    }
    pub fn drawdown(&self) -> f64 {
        self.equity_high - self.equity
    }
}

#[derive(Debug, PartialEq)]
pub enum GateDecision {
    Allow,
    Deny(String),
}

/// The risk gate: pure function of limits, ledger, and the intent.
pub fn risk_gate(limits: &RiskLimits, ledger: &Ledger, intent: &Intent, ref_px: f64) -> GateDecision {
    let pos = ledger.positions.get(&intent.symbol).copied().unwrap_or(0.0);
    let after = pos + intent.side.sign() * intent.qty;
    if after.abs() > limits.max_position_qty + 1e-12 {
        return GateDecision::Deny(format!("position after order {after} exceeds limit {}", limits.max_position_qty));
    }
    let notional = intent.qty * intent.limit_px.unwrap_or(ref_px);
    if notional > limits.max_order_notional {
        return GateDecision::Deny(format!("order notional {notional:.2} exceeds limit {:.2}", limits.max_order_notional));
    }
    if -ledger.realized_pnl > limits.max_daily_loss {
        return GateDecision::Deny(format!("daily loss {:.2} exceeds limit {:.2}", -ledger.realized_pnl, limits.max_daily_loss));
    }
    if ledger.drawdown() > limits.max_drawdown {
        return GateDecision::Deny(format!("drawdown {:.2} exceeds limit {:.2}", ledger.drawdown(), limits.max_drawdown));
    }
    GateDecision::Allow
}

/// The kill switch: a file on disk halts everything, and so does a drawdown breach.
pub fn kill_switch(path: &std::path::Path, limits: &RiskLimits, ledger: &Ledger) -> Option<String> {
    if path.exists() {
        return Some(format!("kill file present at {}", path.display()));
    }
    if ledger.drawdown() > limits.max_drawdown {
        return Some(format!("drawdown {:.2} > {:.2}", ledger.drawdown(), limits.max_drawdown));
    }
    None
}

/// What comes back from a venue, in the order it arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VenueEvent {
    Ack { client_id: String },
    Fill { client_id: String, fill: Fill },
    Cancelled { client_id: String },
    Rejected { client_id: String, reason: String },
    Disconnected,
}

pub trait Venue {
    fn submit(&mut self, order: &Order) -> anyhow::Result<()>;
    fn cancel(&mut self, client_id: &str) -> anyhow::Result<()>;
    /// Drain events that have arrived.
    fn poll(&mut self) -> Vec<VenueEvent>;
}

/// A simulated exchange with chaos knobs, deterministic for a seed.
pub struct SimExchange {
    rng: rand_pcg::Pcg64Mcg,
    pub partial_fill_prob: f64,
    pub dup_ack_prob: f64,
    pub drop_ack_prob: f64,
    pub disconnect_prob: f64,
    pub reject_prob: f64,
    pub mid_px: f64,
    queue: Vec<VenueEvent>,
    seq: u64,
    /// Orders the venue knows about, so a duplicate submit is recognised and not double-filled.
    known: BTreeMap<String, f64>,
}

impl SimExchange {
    pub fn new(seed: u64, mid_px: f64) -> Self {
        use rand::SeedableRng;
        Self { rng: rand_pcg::Pcg64Mcg::seed_from_u64(seed), partial_fill_prob: 0.3, dup_ack_prob: 0.2, drop_ack_prob: 0.1, disconnect_prob: 0.05, reject_prob: 0.05, mid_px, queue: Vec::new(), seq: 0, known: BTreeMap::new() }
    }
    fn chance(&mut self, p: f64) -> bool {
        use rand::Rng;
        self.rng.random::<f64>() < p
    }
}

impl Venue for SimExchange {
    fn submit(&mut self, order: &Order) -> anyhow::Result<()> {
        let id = order.client_id.clone();
        if self.known.contains_key(&id) {
            // Idempotent: the venue has it; resend the ack only.
            self.queue.push(VenueEvent::Ack { client_id: id });
            return Ok(());
        }
        self.known.insert(id.clone(), order.intent.qty);
        if self.chance(self.reject_prob) {
            self.queue.push(VenueEvent::Rejected { client_id: id, reason: "simulated reject".into() });
            return Ok(());
        }
        if !self.chance(self.drop_ack_prob) {
            self.queue.push(VenueEvent::Ack { client_id: id.clone() });
            if self.chance(self.dup_ack_prob) {
                self.queue.push(VenueEvent::Ack { client_id: id.clone() });
            }
        }
        let px = order.intent.limit_px.unwrap_or(self.mid_px);
        let qty = order.intent.qty;
        if self.chance(self.partial_fill_prob) {
            let a = (qty * 0.4 * 1e8).round() / 1e8;
            self.seq += 1;
            let f1 = Fill { fill_id: format!("f{}", self.seq), qty: a, px };
            self.queue.push(VenueEvent::Fill { client_id: id.clone(), fill: f1.clone() });
            if self.chance(self.dup_ack_prob) {
                self.queue.push(VenueEvent::Fill { client_id: id.clone(), fill: f1 });
            }
            self.seq += 1;
            self.queue.push(VenueEvent::Fill { client_id: id.clone(), fill: Fill { fill_id: format!("f{}", self.seq), qty: qty - a, px } });
        } else {
            self.seq += 1;
            self.queue.push(VenueEvent::Fill { client_id: id.clone(), fill: Fill { fill_id: format!("f{}", self.seq), qty, px } });
        }
        if self.chance(self.disconnect_prob) {
            self.queue.push(VenueEvent::Disconnected);
        }
        Ok(())
    }
    fn cancel(&mut self, client_id: &str) -> anyhow::Result<()> {
        self.queue.push(VenueEvent::Cancelled { client_id: client_id.to_string() });
        Ok(())
    }
    fn poll(&mut self) -> Vec<VenueEvent> {
        std::mem::take(&mut self.queue)
    }
}

/// The engine: orders by client ID, the ledger, limits, and the gate in front of every submit.
pub struct Engine {
    pub orders: BTreeMap<String, Order>,
    pub ledger: Ledger,
    pub limits: RiskLimits,
    pub kill_file: std::path::PathBuf,
    pub log: Vec<String>,
}

impl Engine {
    pub fn new(limits: RiskLimits, kill_file: std::path::PathBuf) -> Self {
        Self { orders: BTreeMap::new(), ledger: Ledger::default(), limits, kill_file, log: Vec::new() }
    }

    /// Submit an intent. Returns the client ID. A repeat of the same intent
    /// returns the same ID and submits nothing new.
    pub fn submit<V: Venue>(&mut self, venue: &mut V, intent: Intent, ref_px: f64) -> anyhow::Result<Result<String, String>> {
        let id = client_id(&intent);
        if self.orders.contains_key(&id) {
            self.log.push(format!("{id}: duplicate intent, not resubmitted"));
            return Ok(Ok(id));
        }
        if let Some(why) = kill_switch(&self.kill_file, &self.limits, &self.ledger) {
            self.log.push(format!("{id}: HALTED {why}"));
            return Ok(Err(why));
        }
        if let GateDecision::Deny(why) = risk_gate(&self.limits, &self.ledger, &intent, ref_px) {
            self.log.push(format!("{id}: denied {why}"));
            return Ok(Err(why));
        }
        let mut order = Order::new(intent);
        order.on_submitted()?;
        venue.submit(&order)?;
        self.log.push(format!("{id}: submitted"));
        self.orders.insert(id.clone(), order);
        Ok(Ok(id))
    }

    /// Apply venue events. Unknown IDs and illegal transitions are logged and returned, never swallowed.
    pub fn apply_events(&mut self, events: Vec<VenueEvent>) -> Vec<String> {
        let mut problems = Vec::new();
        for ev in events {
            let r: Result<(), String> = match &ev {
                VenueEvent::Ack { client_id } => self.with(client_id, |o| o.on_acked()),
                VenueEvent::Fill { client_id, fill } => {
                    let (sym, side) = match self.orders.get(client_id) { Some(o) => (o.intent.symbol.clone(), o.intent.side), None => { problems.push(format!("fill for unknown order {client_id}")); continue; } };
                    let before = self.orders[client_id].fills.len();
                    let r = self.with(client_id, |o| o.on_fill(fill.clone()));
                    if r.is_ok() && self.orders[client_id].fills.len() > before {
                        self.ledger.apply_fill(&sym, side, fill.qty, fill.px);
                    }
                    r
                }
                VenueEvent::Cancelled { client_id } => self.with(client_id, |o| o.on_cancelled()),
                VenueEvent::Rejected { client_id, reason } => self.with(client_id, |o| o.on_rejected(reason)),
                VenueEvent::Disconnected => { self.log.push("venue disconnected; open orders remain open until reconciled".into()); Ok(()) }
            };
            if let Err(e) = r {
                self.log.push(format!("PROBLEM {e}"));
                problems.push(e);
            }
        }
        problems
    }

    fn with(&mut self, id: &str, f: impl FnOnce(&mut Order) -> Result<(), TransitionError>) -> Result<(), String> {
        match self.orders.get_mut(id) {
            Some(o) => f(o).map_err(|e| format!("{id}: {e}")),
            None => Err(format!("event for unknown order {id}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(ts: i64, qty: f64) -> Intent {
        Intent { strategy: "t".into(), symbol: "BTCUSDT".into(), side: Side::Buy, qty, limit_px: None, decision_ts_ms: ts }
    }
    fn limits() -> RiskLimits {
        RiskLimits { max_position_qty: 10.0, max_order_notional: 1_000_000.0, max_daily_loss: 1_000.0, max_drawdown: 500.0 }
    }
    fn kill_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fillgate-kill-{}-{}", std::process::id(), rand_u()))
    }
    fn rand_u() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        C.fetch_add(1, Ordering::SeqCst)
    }

    #[test]
    fn same_intent_same_id_and_no_double_position() {
        let mut e = Engine::new(limits(), kill_path());
        let mut v = SimExchange::new(1, 100.0);
        v.reject_prob = 0.0; v.drop_ack_prob = 0.0; v.disconnect_prob = 0.0;
        let a = e.submit(&mut v, intent(1, 1.0), 100.0).unwrap().unwrap();
        let b = e.submit(&mut v, intent(1, 1.0), 100.0).unwrap().unwrap();
        assert_eq!(a, b);
        let problems = e.apply_events(v.poll());
        assert!(problems.is_empty(), "{problems:?}");
        assert!((e.ledger.positions["BTCUSDT"] - 1.0).abs() < 1e-9, "position doubled");
    }

    #[test]
    fn duplicate_fill_ids_do_not_double_count() {
        let mut o = Order::new(intent(1, 2.0));
        o.on_submitted().unwrap();
        o.on_acked().unwrap();
        o.on_acked().unwrap(); // duplicate ack is fine
        o.on_fill(Fill { fill_id: "x".into(), qty: 1.0, px: 10.0 }).unwrap();
        o.on_fill(Fill { fill_id: "x".into(), qty: 1.0, px: 10.0 }).unwrap(); // duplicate ignored
        assert_eq!(o.state, State::PartiallyFilled { filled: 1.0 });
        o.on_fill(Fill { fill_id: "y".into(), qty: 1.0, px: 12.0 }).unwrap();
        assert!(matches!(o.state, State::Filled { avg_px, .. } if (avg_px - 11.0).abs() < 1e-9));
        assert!(o.on_fill(Fill { fill_id: "z".into(), qty: 0.1, px: 12.0 }).is_err(), "fill after filled is illegal");
    }

    #[test]
    fn overfill_is_an_error_not_a_warning() {
        let mut o = Order::new(intent(1, 1.0));
        o.on_submitted().unwrap();
        assert!(o.on_fill(Fill { fill_id: "a".into(), qty: 1.5, px: 1.0 }).is_err());
    }

    #[test]
    fn risk_gate_denies_over_limits() {
        let l = limits();
        let mut led = Ledger::default();
        led.positions.insert("BTCUSDT".into(), 9.5);
        assert!(matches!(risk_gate(&l, &led, &intent(1, 1.0), 100.0), GateDecision::Deny(_)));
        let big = Intent { qty: 20_000.0, ..intent(1, 1.0) };
        assert!(matches!(risk_gate(&l, &Ledger::default(), &big, 100.0), GateDecision::Deny(_)));
        assert_eq!(risk_gate(&l, &Ledger::default(), &intent(1, 1.0), 100.0), GateDecision::Allow);
    }

    #[test]
    fn kill_file_halts_before_the_venue_is_touched() {
        let k = kill_path();
        std::fs::write(&k, "halt").unwrap();
        let mut e = Engine::new(limits(), k.clone());
        let mut v = SimExchange::new(2, 100.0);
        let r = e.submit(&mut v, intent(1, 1.0), 100.0).unwrap();
        assert!(r.is_err());
        assert!(v.poll().is_empty(), "venue must not see the order");
        std::fs::remove_file(k).unwrap();
    }

    #[test]
    fn ledger_realizes_pnl_on_the_closed_part() {
        let mut l = Ledger::default();
        l.apply_fill("X", Side::Buy, 2.0, 100.0);
        l.apply_fill("X", Side::Sell, 1.0, 110.0);
        assert!((l.realized_pnl - 10.0).abs() < 1e-9);
        assert!((l.positions["X"] - 1.0).abs() < 1e-9);
        l.apply_fill("X", Side::Sell, 1.0, 90.0);
        assert!((l.realized_pnl - 0.0).abs() < 1e-9);
        assert!((l.drawdown() - 10.0).abs() < 1e-9, "equity high 10, now 0");
    }

    #[test]
    fn chaos_run_never_corrupts_the_ledger() {
        // 200 intents through a hostile venue; every fill is accounted once and the position never exceeds the limit.
        for seed in 0..5u64 {
            let mut e = Engine::new(RiskLimits { max_position_qty: 50.0, max_order_notional: 1e9, max_daily_loss: 1e9, max_drawdown: 1e9 }, kill_path());
            let mut v = SimExchange::new(seed, 100.0);
            for i in 0..200 {
                let side = if i % 3 == 0 { Side::Sell } else { Side::Buy };
                let _ = e.submit(&mut v, Intent { side, ..intent(i, 1.0) }, 100.0).unwrap();
                let problems = e.apply_events(v.poll());
                assert!(problems.is_empty(), "seed {seed}: {problems:?}");
            }
            let filled: f64 = e.orders.values().map(|o| o.fills.iter().map(|f| f.qty).sum::<f64>()).sum();
            let net: f64 = e.orders.values().map(|o| o.intent.side.sign() * o.fills.iter().map(|f| f.qty).sum::<f64>()).sum();
            assert!((e.ledger.positions.get("BTCUSDT").copied().unwrap_or(0.0) - net).abs() < 1e-6);
            assert!(filled <= 200.0 + 1e-9);
            assert!(e.ledger.positions.values().all(|p| p.abs() <= 50.0 + 1e-9));
        }
    }
}
