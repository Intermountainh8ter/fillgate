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

## Venues

| Venue | Mode | Status |
| --- | --- | --- |
| Simulated exchange | chaos, seeded | implemented, tested |
| Alpaca paper | REST | `check` implemented; orders next |
| Binance spot testnet | REST, HMAC | `check` implemented; orders next |
| Hyperliquid testnet | signed requests | planned |

## What this is not

It holds no production keys, has no live venue, and makes no claim about profitability. The weekly paper record that will use it is pre-registered separately before its first order.

## License

MIT.
