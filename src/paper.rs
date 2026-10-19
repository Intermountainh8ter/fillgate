//! The weekly paper-trading record (docs/PREREGISTRATION-paper-record.md).
//!
//! `run_once` is one hourly tick: read the venue's completed hourly bars,
//! compute the Donchian target with the `n` that walkforward's locked
//! procedure chose, submit at most one market order through the engine's
//! gate, wait for it to resolve, and append one decision record to this ISO
//! week's file. State (ledger, orders, buy-and-hold entry) persists in
//! `state.json` so the process can be one-shot under a timer. The same bar
//! never produces two decisions: the decision timestamp is part of the
//! client ID and the state remembers the last bar decided.

use crate::alpaca::{AlpacaPaper, Bar, JournalEntry};
use crate::{Engine, Fill, Intent, Ledger, Order, RiskLimits, Side, State, Venue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Round-trip haircut in basis points (walkforward's cost model), charged as half per fill.
pub const HAIRCUT_BPS_ROUND_TRIP: f64 = 15.0;

/// Parse an RFC 3339 UTC timestamp with optional fractional seconds into Unix milliseconds.
pub fn rfc3339_to_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (hms, frac) = match time.split_once('.') { Some((a, f)) => (a, f), None => (time, "") };
    let mut t = hms.split(':');
    let (h, mi, sec): (u64, u64, u64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let ms: u64 = if frac.is_empty() { 0 } else { format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()? };
    Some((days_from_civil(y, m, day) as u64 * 86400 + h * 3600 + mi * 60 + sec) * 1000 + ms)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO 8601 week label, e.g. `2026-W43`, for a Unix millisecond timestamp.
pub fn iso_week(unix_ms: u64) -> String {
    let days = (unix_ms / 86_400_000) as i64;
    let weekday = ((days + 3).rem_euclid(7)) + 1; // 1 = Monday ... 7 = Sunday (1970-01-01 was a Thursday)
    let thursday = days + (4 - weekday);
    let (y, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(y, 1, 1);
    let week = (thursday - jan1) / 7 + 1;
    format!("{y}-W{week:02}")
}

pub fn utc_day(unix_ms: u64) -> String {
    let (y, m, d) = civil_from_days((unix_ms / 86_400_000) as i64);
    format!("{y}-{m:02}-{d:02}")
}

/// Donchian target on the last bar, exactly as walkforward computes it: long if the close is
/// above the highest high of the previous `n` bars, flat if below the lowest low, otherwise hold
/// (None). Needs at least n+1 bars.
pub fn donchian_target(bars: &[Bar], n: usize) -> Option<f64> {
    let t = bars.len().checked_sub(1)?;
    if t < n { return None; }
    let hi = bars[t - n..t].iter().map(|b| b.high).fold(f64::MIN, f64::max);
    let lo = bars[t - n..t].iter().map(|b| b.low).fold(f64::MAX, f64::min);
    let c = bars[t].close;
    if c > hi { Some(1.0) } else if c < lo { Some(0.0) } else { None }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperState {
    pub venue: String,
    pub symbol: String,
    pub unit_qty: f64,
    pub started_unix_ms: u64,
    pub strategy: String,
    pub n: f64,
    pub run_file_sha256: String,
    pub ledger: Ledger,
    pub orders: BTreeMap<String, Order>,
    pub last_decision_ts_ms: Option<u64>,
    pub bh_entry_px: Option<f64>,
    pub bh_entry_ts_ms: Option<u64>,
    pub day: String,
    pub day_start_realized: f64,
    pub halted: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub unix_ms: u64,
    pub week: String,
    pub bar_ts_ms: u64,
    pub bar_close: f64,
    pub n: f64,
    pub signal: String,
    pub position_before: f64,
    pub position_after: f64,
    pub intent: Option<Intent>,
    pub client_id: Option<String>,
    pub gate: String,
    pub order_state: Option<State>,
    pub fills: Vec<Fill>,
    pub fill_notional: f64,
    pub journal: Vec<JournalEntry>,
    pub realized_cumulative: f64,
    pub mark_px: f64,
    pub equity_mtm: f64,
    pub bh_entry_px: Option<f64>,
    pub account: serde_json::Value,
    pub venue_position_qty: Option<f64>,
    pub manual: bool,
}

pub struct RunArgs {
    pub venue_name: String,
    pub symbol: String,
    pub unit_qty: f64,
    pub run_file: PathBuf,
    pub out_dir: PathBuf,
    pub wait_secs: u64,
}

pub fn limits() -> RiskLimits {
    // Pre-registration: max position 0.01, max order notional 2,000 USD, drawdown halt 300 USD.
    // The daily-loss limit (100 USD) is applied in `run_once` against today's realized P&L because
    // the engine's ledger is cumulative across days here.
    RiskLimits { max_position_qty: 0.01, max_order_notional: 2000.0, max_daily_loss: f64::INFINITY, max_drawdown: 300.0 }
}

pub const MAX_DAILY_LOSS_USD: f64 = 100.0;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sha256_file(p: &Path) -> anyhow::Result<String> {
    use sha2::Digest;
    Ok(hex::encode(sha2::Sha256::digest(std::fs::read(p)?)))
}

/// `n` from the last walk-forward window's `chosen` in a walkforward run file.
pub fn chosen_n(run_file: &Path) -> anyhow::Result<(f64, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(run_file)?)?;
    let windows = v["windows"].as_array().ok_or_else(|| anyhow::anyhow!("run file has no windows"))?;
    let last = windows.last().ok_or_else(|| anyhow::anyhow!("run file has zero windows"))?;
    let n = last["chosen"]["n"].as_f64().ok_or_else(|| anyhow::anyhow!("last window has no chosen.n"))?;
    Ok((n, sha256_file(run_file)?))
}

fn load_state(dir: &Path, a: &RunArgs, n: f64, run_sha: String) -> anyhow::Result<PaperState> {
    let p = dir.join("state.json");
    if p.exists() {
        let mut s: PaperState = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
        anyhow::ensure!(s.symbol == a.symbol && s.venue == a.venue_name, "state.json is for {}/{}, not {}/{}", s.venue, s.symbol, a.venue_name, a.symbol);
        if s.n != n || s.run_file_sha256 != run_sha {
            // Quarterly re-selection by walkforward's rule shows up here as a new run file.
            s.n = n;
            s.run_file_sha256 = run_sha;
        }
        return Ok(s);
    }
    let now = now_ms();
    Ok(PaperState { venue: a.venue_name.clone(), symbol: a.symbol.clone(), unit_qty: a.unit_qty, started_unix_ms: now, strategy: "donchian".into(), n, run_file_sha256: run_sha, ledger: Ledger::default(), orders: BTreeMap::new(), last_decision_ts_ms: None, bh_entry_px: None, bh_entry_ts_ms: None, day: utc_day(now), day_start_realized: 0.0, halted: None })
}

fn save_state(dir: &Path, s: &PaperState) -> anyhow::Result<()> {
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(tmp, dir.join("state.json"))?;
    Ok(())
}

fn append_record(dir: &Path, r: &DecisionRecord) -> anyhow::Result<PathBuf> {
    let p = dir.join(format!("{}.json", r.week));
    let mut arr: Vec<serde_json::Value> = if p.exists() { serde_json::from_str(&std::fs::read_to_string(&p)?)? } else { Vec::new() };
    arr.push(serde_json::to_value(r)?);
    std::fs::write(&p, serde_json::to_string_pretty(&arr)?)?;
    Ok(p)
}

/// One hourly tick. Returns the record written (also appended to the week file).
pub fn run_once(a: &RunArgs, venue: &mut AlpacaPaper) -> anyhow::Result<DecisionRecord> {
    let dir = a.out_dir.join(&a.venue_name);
    std::fs::create_dir_all(&dir)?;
    let (n, run_sha) = chosen_n(&a.run_file)?;
    let mut st = load_state(&dir, a, n, run_sha)?;
    let now = now_ms();

    // Day roll for the daily-loss limit.
    let today = utc_day(now);
    if st.day != today {
        st.day = today;
        st.day_start_realized = st.ledger.realized_pnl;
    }

    let bars = venue.hourly_bars(&a.symbol, n as usize + 2)?;
    anyhow::ensure!(bars.len() >= n as usize + 1, "only {} completed bars, need {}", bars.len(), n as usize + 1);
    let bar = bars.last().unwrap().clone();
    if st.last_decision_ts_ms == Some(bar.ts_ms) {
        anyhow::bail!("bar {} already decided; nothing to do", bar.ts_ms);
    }
    if st.bh_entry_px.is_none() {
        st.bh_entry_px = Some(bar.close);
        st.bh_entry_ts_ms = Some(bar.ts_ms);
    }

    let target = donchian_target(&bars, n as usize);
    let position_before = st.ledger.positions.get(&a.symbol).copied().unwrap_or(0.0);
    let (signal, desired) = match target { Some(1.0) => ("long", st.unit_qty), Some(_) => ("flat", 0.0), None => ("hold", position_before) };
    let mut rec = DecisionRecord {
        unix_ms: now, week: iso_week(now), bar_ts_ms: bar.ts_ms, bar_close: bar.close, n, signal: signal.into(),
        position_before, position_after: position_before, intent: None, client_id: None, gate: "no-op".into(),
        order_state: None, fills: Vec::new(), fill_notional: 0.0, journal: Vec::new(), realized_cumulative: st.ledger.realized_pnl,
        mark_px: bar.close, equity_mtm: 0.0, bh_entry_px: st.bh_entry_px, account: serde_json::Value::Null, venue_position_qty: None, manual: false,
    };

    let mut engine = Engine::new(limits(), dir.join("KILL"));
    engine.ledger = st.ledger.clone();
    engine.orders = st.orders.clone();

    let delta = desired - position_before;
    if delta.abs() > 1e-9 {
        let today_loss = -(st.ledger.realized_pnl - st.day_start_realized);
        if today_loss > MAX_DAILY_LOSS_USD {
            rec.gate = format!("denied: daily loss {today_loss:.2} exceeds {MAX_DAILY_LOSS_USD:.2}");
        } else if let Some(h) = &st.halted {
            rec.gate = format!("halted: {h}");
        } else {
            let intent = Intent { strategy: format!("donchian-n{}", n as usize), symbol: a.symbol.clone(), side: if delta > 0.0 { Side::Buy } else { Side::Sell }, qty: delta.abs(), limit_px: None, decision_ts_ms: bar.ts_ms as i64 };
            rec.intent = Some(intent.clone());
            match engine.submit(venue, intent, bar.close)? {
                Ok(id) => {
                    rec.client_id = Some(id.clone());
                    rec.gate = "submitted".into();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(a.wait_secs);
                    loop {
                        let problems = engine.apply_events(venue.poll());
                        for p in problems { engine.log.push(format!("PROBLEM {p}")); }
                        let done = engine.orders.get(&id).map_or(true, |o| o.is_terminal());
                        if done || std::time::Instant::now() > deadline { break; }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                    if let Some(o) = engine.orders.get(&id) {
                        rec.order_state = Some(o.state.clone());
                        rec.fills = o.fills.clone();
                        rec.fill_notional = o.fills.iter().map(|f| f.qty * f.px).sum();
                    }
                }
                Err(why) => {
                    rec.gate = if why.starts_with("kill") || why.starts_with("drawdown") { st.halted = Some(why.clone()); format!("halted: {why}") } else { format!("denied: {why}") };
                }
            }
        }
    }

    rec.journal = std::mem::take(&mut venue.journal);
    rec.account = venue.account().unwrap_or(serde_json::Value::Null);
    rec.venue_position_qty = venue.position_qty(&a.symbol).ok();
    rec.journal.extend(std::mem::take(&mut venue.journal));

    st.ledger = engine.ledger.clone();
    st.orders = engine.orders.clone();
    st.last_decision_ts_ms = Some(bar.ts_ms);
    rec.position_after = st.ledger.positions.get(&a.symbol).copied().unwrap_or(0.0);
    rec.realized_cumulative = st.ledger.realized_pnl;
    let avg = st.ledger.avg_px.get(&a.symbol).copied().unwrap_or(0.0);
    rec.equity_mtm = st.ledger.realized_pnl + rec.position_after * (bar.close - avg);
    append_record(&dir, &rec)?;
    save_state(&dir, &st)?;
    Ok(rec)
}

#[derive(Debug, Clone, Serialize)]
pub struct WeekRow {
    pub venue: String,
    pub week: String,
    pub decisions: usize,
    pub orders: usize,
    pub fills: usize,
    pub raw_realized_delta: f64,
    pub haircut_cumulative: f64,
    pub equity_mtm: f64,
    pub bh_haircut: f64,
    pub halts_or_manual: usize,
}

/// Read every `<venue>/<week>.json` under `out_dir` and render the README block.
pub fn report(out_dir: &Path, readme: &Path) -> anyhow::Result<()> {
    let mut rows: Vec<WeekRow> = Vec::new();
    let mut venues: Vec<_> = std::fs::read_dir(out_dir)?.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).collect();
    venues.sort_by_key(|e| e.file_name());
    for v in venues {
        let venue = v.file_name().to_string_lossy().to_string();
        let mut weeks: Vec<PathBuf> = std::fs::read_dir(v.path())?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().map_or(false, |f| f.to_string_lossy().contains("-W") && f.to_string_lossy().ends_with(".json"))).collect();
        weeks.sort();
        let (mut prev_realized, mut haircut_total, mut bh_entry, mut unit) = (0.0f64, 0.0f64, None::<f64>, 0.0f64);
        for w in weeks {
            let recs: Vec<DecisionRecord> = serde_json::from_str(&std::fs::read_to_string(&w)?)?;
            if recs.is_empty() { continue; }
            let last = recs.last().unwrap();
            let fills: usize = recs.iter().map(|r| r.fills.len()).sum();
            let notional: f64 = recs.iter().map(|r| r.fill_notional).sum();
            haircut_total += notional * HAIRCUT_BPS_ROUND_TRIP / 2.0 / 10_000.0;
            if bh_entry.is_none() { bh_entry = last.bh_entry_px; }
            if unit == 0.0 { unit = recs.iter().filter_map(|r| r.intent.as_ref()).map(|i| i.qty).fold(0.0, f64::max); }
            let bh = bh_entry.map(|e| unit * (last.mark_px - e) - unit * e * HAIRCUT_BPS_ROUND_TRIP / 10_000.0).unwrap_or(0.0);
            rows.push(WeekRow {
                venue: venue.clone(), week: last.week.clone(), decisions: recs.len(),
                orders: recs.iter().filter(|r| r.client_id.is_some()).count(), fills,
                raw_realized_delta: last.realized_cumulative - prev_realized,
                haircut_cumulative: last.equity_mtm - haircut_total,
                equity_mtm: last.equity_mtm, bh_haircut: bh,
                halts_or_manual: recs.iter().filter(|r| r.manual || r.gate.starts_with("halted")).count(),
            });
            prev_realized = last.realized_cumulative;
        }
    }
    let mut t = String::new();
    t.push_str(&format!("Source: `{}/<venue>/<week>.json`, one record per hourly decision. Haircut: {} bps round trip charged as half per fill; buy-and-hold is one unit entered at the first decision bar's close with the same haircut, marked at the latest decision bar's close. The pre-registered claim (haircut P&L above buy-and-hold over the full period) is read only after 12 weeks.\n\n", out_dir.display(), HAIRCUT_BPS_ROUND_TRIP));
    t.push_str("| venue | week | decisions | orders | fills | raw realized (USD, week) | haircut equity (USD, cumulative) | buy-and-hold haircut (USD, cumulative) | halts or manual |\n|---|---|---|---|---|---|---|---|---|\n");
    for r in &rows {
        t.push_str(&format!("| {} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {} |\n", r.venue, r.week, r.decisions, r.orders, r.fills, r.raw_realized_delta, r.haircut_cumulative, r.bh_haircut, r.halts_or_manual));
    }
    if rows.is_empty() { t.push_str("| (no records yet) | | | | | | | | |\n"); }
    let weeks_done = rows.iter().map(|r| r.week.clone()).collect::<std::collections::BTreeSet<_>>().len();
    t.push_str(&format!("\nWeeks recorded: {weeks_done} of 12 before the claim is read.\n"));
    let readme_text = std::fs::read_to_string(readme)?;
    let (start, end) = ("<!-- PAPER:START -->", "<!-- PAPER:END -->");
    let (a, b) = (readme_text.find(start).ok_or_else(|| anyhow::anyhow!("README lacks {start}"))?, readme_text.find(end).ok_or_else(|| anyhow::anyhow!("README lacks {end}"))?);
    let out = format!("{}{}\n{}\n{}", &readme_text[..a], start, t, &readme_text[b..]);
    std::fs::write(readme, out)?;
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: u64, h: f64, l: f64, c: f64) -> Bar { Bar { ts_ms: ts, open: c, high: h, low: l, close: c, volume: 1.0 } }

    #[test]
    fn donchian_matches_walkforward_rule() {
        let mut bars: Vec<Bar> = (0..5).map(|i| bar(i, 10.0 + i as f64, 5.0 + i as f64, 8.0)).collect();
        // previous 3 bars (indices 2..5): highs 12,13,14 -> hi 14; lows 7,8,9 -> lo 7
        bars.push(bar(5, 20.0, 1.0, 14.0));
        assert_eq!(donchian_target(&bars, 3), None, "close equal to the channel high is a hold");
        bars.last_mut().unwrap().close = 15.0;
        assert_eq!(donchian_target(&bars, 3), Some(1.0));
        bars.last_mut().unwrap().close = 5.0;
        assert_eq!(donchian_target(&bars, 3), Some(0.0));
        bars.last_mut().unwrap().close = 9.0;
        assert_eq!(donchian_target(&bars, 3), None);
        assert_eq!(donchian_target(&bars[..3], 3), None);
    }

    #[test]
    fn iso_week_and_day_labels() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(iso_week(0), "1970-W01");
        // 2026-10-19 is a Monday in ISO week 43.
        assert_eq!(utc_day(1792422245123), "2026-10-19");
        assert_eq!(iso_week(1792422245123), "2026-W43");
        // 2021-01-03 (Sunday) belongs to 2020-W53.
        assert_eq!(iso_week(1609632000000), "2020-W53");
        assert_eq!(rfc3339_to_ms("2026-10-19T15:04:05.123456Z"), Some(1792422245123));
    }
}
