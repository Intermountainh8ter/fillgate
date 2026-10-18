# Pre-registration: the weekly paper-trading record

Written 2026-10-18, before the first paper order. Append-only below the line.

## What is being recorded

A forward, out-of-sample, paper-money record of one strategy whose parameters were selected by
walkforward's locked procedure, placed through fillgate on paper venues, committed weekly with the
losing weeks reported exactly like the winning ones.

## Strategy and parameters

- Strategy: Donchian breakout on hourly bars, long-flat, as implemented in walkforward
  (`donchian`, lookback `n`), because it had the highest out-of-sample Sharpe of the four
  pre-registered studies (0.52 on BTC) and still did not beat buy-and-hold. That is the point:
  the record will show whether a procedure-selected strategy survives contact with live paper
  fills, not whether a winner was cherry-picked.
- Parameter: the `n` chosen by the most recent in-sample window of `results/prereg-btc-donchian.json`
  at the time this record starts, read from `results/run-btc-donchian.json` (last window's
  `chosen`), re-selected every quarter (2,190 bars) by the same rule, never by hand.
- Instrument: BTC/USD on Alpaca paper (crypto), and BTC perp on Hyperliquid testnet once that
  adapter exists. Each venue is its own row; no pooling.
- Size: one unit = 0.01 BTC per signal. No pyramiding, no leverage.

## Execution rules, fixed now

- Signals computed on the close of each hourly bar from the venue's own bar data; the order is
  submitted at the open of the next bar as a market order (walkforward's fill assumption).
- Client order IDs are derived from the decision (fillgate `client_id`), so a retry can never
  double an order.
- Risk gate: max position 0.01 BTC, max order notional 2,000 USD, max daily loss 100 USD paper,
  drawdown halt 300 USD paper. A halt is a result and is reported as such; the record does not
  resume until the halt and its cause are committed.
- No manual intervention. If I touch an order by hand the week is marked `manual` and excluded
  from the headline, but still committed.

## Costs and the haircut

Paper fills are optimistic: no queue, no impact. Every fill is additionally charged a pre-fixed
haircut of 15 basis points round trip (the walkforward cost model). Both the raw paper P&L and the
haircut P&L are reported; the haircut number is the headline.

## Reporting

- Every order, acknowledgement, fill and gate decision is logged with the fillgate client ID and a
  SHA-256 of the raw venue response, and committed weekly under `results/paper/<venue>/<week>.json`.
- The README table is rendered from those files by a `report` subcommand, never typed.
- Buy-and-hold over the same weeks, with the same haircut on its single entry, sits beside it.
- The record runs for at least 12 weeks before any claim is made; the pre-registered claim is only
  "haircut P&L above buy-and-hold over the full period", supported or not.

## What this record cannot show

Real fills, real slippage, real exchange outages, or anything about live profitability. It shows
whether the whole pipeline (data, signal, gate, venue, logging, weekly reporting) runs unattended
and honestly for three months.
