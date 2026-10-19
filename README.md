# fillgate

**The gate in front of every order.** An order passes, in this order, through an idempotent client ID derived from the decision (so a retry can never place a second order), a risk gate (position, order notional, daily loss, drawdown), a kill switch (a file on disk or a drawdown breach), and only then a venue. Every state transition is validated; an illegal one is an error, never a warning. A simulated exchange with seeded chaos exists to break the engine on purpose.

![language: Rust](https://img.shields.io/badge/language-Rust-blue) ![license: MIT](https://img.shields.io/badge/license-MIT-blue) ![venues: paper only](https://img.shields.io/badge/venues-paper%20only-informational)

## What the tests pin

- The same intent submitted twice yields the same client ID and one position, not two.
- A duplicate acknowledgement is a no-op; a duplicate fill ID is ignored; a fill past the order's quantity is an error; a fill after `Filled` is illegal.
- The risk gate denies an order that would breach the position limit, the notional limit, the daily-loss limit, or the drawdown limit.
- The kill file halts the engine before the venue is touched.
- Realized P&L is booked only on the closed part of a position.
- Two hundred intents through the chaos exchange (partial fills, duplicate acks, dropped acks, rejects, disconnects) across five seeds never corrupt the ledger: every fill counted once, position equal to net filled quantity, limits never exceeded.

## Usage

```
fillgate sim --seed 7 --intents 100          # chaos run; exit 1 if any problem is recorded
fillgate check --venue alpaca                # reads ~/.trading/alpaca.env, calls the account endpoint, prints a masked summary
fillgate check --venue binance               # reads ~/.trading/binance.env (testnet only)
```

`check` refuses a non-paper Alpaca URL and a non-testnet Binance URL. Keys are read from files outside the repository and are never printed, logged or written.

```
fillgate paper --venue alpaca --run ../walkforward/results/run-btc-donchian.json   # one hourly tick of the paper record
fillgate report                                                                    # render results/paper into the block below
```

## The paper record

[docs/PREREGISTRATION-paper-record.md](docs/PREREGISTRATION-paper-record.md) was written before the first paper order. `paper` is one hourly tick: it reads the venue's completed hourly bars, computes the Donchian target with the `n` that walkforward's locked procedure chose for the most recent window, puts at most one market order through the engine's gate (position 0.01 BTC, notional 2,000 USD, daily loss 100 USD, drawdown halt 300 USD), waits for it to resolve, and appends one record to `results/paper/<venue>/<ISO week>.json` with the client ID, every gate decision, every fill, and a SHA-256 of every raw venue response. The same bar can never produce two orders: the bar timestamp is part of the client ID and the state file remembers the last bar decided. `ops/fillgate-paper.timer` runs it at two minutes past each hour.

The Alpaca adapter (`src/alpaca.rs`) is paper-only by construction and reports fills as increments of the venue's running `filled_qty`, priced so the venue's average is preserved. It has not yet placed an order: the key file does not exist on this machine at the time of writing, so the table below is empty until it does.

<!-- PAPER:START -->
| (no records yet) | | | | | | | | |
<!-- PAPER:END -->

## Venues

| Venue | Mode | Status |
| --- | --- | --- |
| Simulated exchange | chaos, seeded | implemented, tested |
| Alpaca paper | REST | `check`, market orders, fills and the hourly paper tick implemented (`src/alpaca.rs`, `src/paper.rs`); no order placed yet, the key file does not exist on this host |
| Binance spot testnet | REST, HMAC | `check` implemented, but the testnet answers HTTP 451 from the United States, so it is unusable from this host |
| Hyperliquid testnet | EIP-712 signed L1 actions (`src/hyperliquid.rs`) | signing matches the venue SDK's reference vectors (unit test); `check` runs live against the testnet info endpoint; IOC market orders and cancel-by-client-ID implemented, not yet exercised with a funded wallet |

## What this is not

It holds no production keys, has no live venue, and makes no claim about profitability. The weekly paper record that will use it is pre-registered separately before its first order.

## License

MIT.
