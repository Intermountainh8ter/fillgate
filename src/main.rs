//! fillgate CLI.
//!
//! `sim` drives the engine through the chaos exchange and prints the ledger
//! and the problem list (expected: empty). `check` reads a venue's key file
//! from `~/.trading/<venue>.env`, calls the account endpoint, and prints a
//! masked summary; it places no orders. Keys are read into memory and never
//! printed, logged, or written.

use clap::{Parser, Subcommand};
use fillgate::*;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "fillgate", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run N scripted intents through the simulated exchange with chaos on.
    Sim {
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 50)]
        intents: u64,
    },
    /// Connectivity and account check for a paper venue. Reads ~/.trading/<venue>.env. No orders.
    Check {
        /// alpaca | binance
        #[arg(long)]
        venue: String,
        /// Directory holding <venue>.env files.
        #[arg(long, default_value = "~/.trading")]
        keys_dir: String,
    },
}

fn load_env_file(path: &PathBuf) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut m = std::collections::BTreeMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            m.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    Ok(m)
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(p)
}

fn require<'a>(m: &'a std::collections::BTreeMap<String, String>, k: &str) -> anyhow::Result<&'a str> {
    m.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("{k} is not set in the key file"))
}

fn check_alpaca(m: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
    let key = require(m, "ALPACA_KEY_ID")?;
    let secret = require(m, "ALPACA_SECRET_KEY")?;
    let base = m.get("ALPACA_BASE_URL").map(|s| s.as_str()).unwrap_or("https://paper-api.alpaca.markets");
    anyhow::ensure!(base.contains("paper"), "refusing: ALPACA_BASE_URL is not the paper endpoint");
    let resp: serde_json::Value = ureq::get(&format!("{base}/v2/account"))
        .set("APCA-API-KEY-ID", key)
        .set("APCA-API-SECRET-KEY", secret)
        .timeout(std::time::Duration::from_secs(20))
        .call()?
        .into_json()?;
    println!("alpaca paper: status={} currency={} equity={} buying_power={} pattern_day_trader={}",
        resp["status"], resp["currency"], resp["equity"], resp["buying_power"], resp["pattern_day_trader"]);
    Ok(())
}

fn hmac_sha256_hex(secret: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn check_binance(m: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
    let key = require(m, "BINANCE_API_KEY")?;
    let secret = require(m, "BINANCE_API_SECRET")?;
    let base = m.get("BINANCE_BASE_URL").map(|s| s.as_str()).unwrap_or("https://testnet.binance.vision");
    anyhow::ensure!(base.contains("testnet"), "refusing: BINANCE_BASE_URL is not the testnet");
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let query = format!("timestamp={ts}&recvWindow=10000");
    let sig = hmac_sha256_hex(secret, &query);
    let resp: serde_json::Value = ureq::get(&format!("{base}/api/v3/account?{query}&signature={sig}"))
        .set("X-MBX-APIKEY", key)
        .timeout(std::time::Duration::from_secs(20))
        .call()?
        .into_json()?;
    let balances: Vec<String> = resp["balances"].as_array().map(|a| a.iter()
        .filter(|b| b["free"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) > 0.0)
        .map(|b| format!("{}={}", b["asset"].as_str().unwrap_or("?"), b["free"].as_str().unwrap_or("0")))
        .collect()).unwrap_or_default();
    println!("binance testnet: canTrade={} nonzero balances: {}", resp["canTrade"], balances.join(" "));
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Sim { seed, intents } => {
            let kill = std::env::temp_dir().join(format!("fillgate-kill-{seed}"));
            let _ = std::fs::remove_file(&kill);
            let mut e = Engine::new(RiskLimits { max_position_qty: 20.0, max_order_notional: 1e7, max_daily_loss: 1e7, max_drawdown: 1e7 }, kill);
            let mut v = SimExchange::new(seed, 100.0);
            let mut denied = 0;
            let mut problems = Vec::new();
            for i in 0..intents as i64 {
                let side = if i % 3 == 0 { Side::Sell } else { Side::Buy };
                let r = e.submit(&mut v, Intent { strategy: "sim".into(), symbol: "SIM".into(), side, qty: 1.0, limit_px: None, decision_ts_ms: i }, 100.0)?;
                if r.is_err() { denied += 1; }
                problems.extend(e.apply_events(v.poll()));
            }
            let states: std::collections::BTreeMap<String, usize> = e.orders.values().fold(Default::default(), |mut m, o| {
                let k = match &o.state { State::Filled { .. } => "filled", State::PartiallyFilled { .. } => "partial", State::Rejected { .. } => "rejected", State::Cancelled { .. } => "cancelled", State::Acked => "acked", State::Submitted => "submitted", State::New => "new" };
                *m.entry(k.to_string()).or_insert(0) += 1; m });
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "seed": seed, "intents": intents, "denied_by_gate": denied, "orders": e.orders.len(), "states": states,
                "position": e.ledger.positions, "realized_pnl": e.ledger.realized_pnl, "problems": problems,
                "log_tail": e.log.iter().rev().take(5).collect::<Vec<_>>()
            }))?);
            if !problems.is_empty() { std::process::exit(1); }
        }
        Cmd::Check { venue, keys_dir } => {
            let path = expand_home(&keys_dir).join(format!("{venue}.env"));
            let m = load_env_file(&path)?;
            match venue.as_str() {
                "alpaca" => check_alpaca(&m)?,
                "binance" => check_binance(&m)?,
                "hyperliquid" => anyhow::bail!("hyperliquid: adapter not implemented yet (needs wallet signing)"),
                other => anyhow::bail!("unknown venue {other}"),
            }
        }
    }
    Ok(())
}
