# Hyperliquid Funding Rate Research

## TL;DR

Quantitative research project in Rust testing whether funding-rate farming on Hyperliquid perpetual swaps is profitable for a retail account at typical bid-ask spreads. The hypothesis was falsified: **0 of 228 parametric strategy combinations produce positive net P&L over a 30-day backtest after realistic transaction costs**.

## Key Finding

| Metric | Value |
|---|---|
| Profitable parameter combinations | **0 / 228** |
| Break-even funding APR (taker entry) | **179%** |
| Break-even funding APR (maker entry) | **95%** |
| Average realized funding APR over hold periods | **~11%** |
| Round-trip cost (taker) | 49 bps (20 spread + 2×4.5 taker + 2×10 slippage) |
| Round-trip cost (maker entry, taker exit) | 26 bps |
| Best single combination (max-hold-only, $200 APR threshold, 12h hold) | **−$0.26 net over 30 days** |

**Root cause.** Extreme funding spikes (>200% APR) are transient by construction — they exist precisely because the market is in temporary imbalance. Realized funding accruals over any reasonable hold period are 3–5× lower than the entry funding APR observed at decision time. The capturable edge is structurally smaller than the round-trip transaction cost across the entire parameter surface.

## What I Built

A production-grade quantitative research stack written in Rust. I designed and implemented every component below from scratch:

- **Real-time data collector.** Long-running daemon (managed by systemd) that polls Hyperliquid's `/info` endpoint every 5 minutes, parses 146+ active perp contexts, and writes batched inserts inside a single SQLite transaction.
- **Time-series storage.** SQLite with WAL-friendly access patterns. Two tables: `funding_snapshots` (collected from `metaAndAssetCtxs`, ~42k rows/day) and `historical_funding` (downloaded from `fundingHistory` for backtesting).
- **Live TUI dashboard.** Five-section ratatui interface with auto-refresh, read-only DB connection (`busy_timeout=5s`), graceful Ctrl+C / `q` shutdown with terminal restoration, panic-hook cleanup.
- **Telegram alerting.** Independent service that diffs current tier classifications against an in-memory snapshot, emits typed events (`NEW SIGNAL`, `TIER UP/DOWN`, `SIGNAL LOST`), and applies anti-spam (30-min cooldown per asset, flip-flop mute after 3 changes in 10 minutes).
- **Backtest engine.** Hour-by-hour simulator over historical funding rates with full cost model (spread, maker/taker fees, slippage, configurable for both entry sides).
- **Parameter sweep.** Cartesian sweep over `{maker, min_funding_apr, hold_hours, max_positions, exit_strategy}` plus a hysteresis-threshold sub-sweep, with a separate verbose run on the best combination.
- **Production deployment.** systemd service units with `KillSignal=SIGINT` for graceful shutdown, `Restart=on-failure` with backoff, environment-based secret injection via `systemctl edit` drop-ins (no secrets in repo).

## Architecture

```
                    ┌──────────────────────────────────┐
                    │  api.hyperliquid.xyz /info       │
                    │  (metaAndAssetCtxs, fundingHistory)│
                    └────────────────┬─────────────────┘
                                     │
                          POST every 5 min       POST batched
                                     │           (backtest only)
                                     ▼
                    ┌──────────────────────────────────┐
                    │  collector  (systemd daemon)     │
                    │  fetch → parse → tx INSERT       │
                    │  graceful shutdown on SIGINT     │
                    └────────────────┬─────────────────┘
                                     │
                                     ▼
                    ┌──────────────────────────────────┐
                    │  ./data/snapshots.db  (SQLite)   │
                    │  ─ funding_snapshots             │
                    │  ─ historical_funding            │
                    └─────┬────────────────────────┬───┘
              read-only   │                        │   read-write
        ┌─────────────────┼──────────┬─────────────┘
        ▼                 ▼          ▼                 ▼
  ┌──────────┐     ┌──────────┐ ┌──────────┐    ┌────────────┐
  │  scan    │     │  watch   │ │  alert-  │    │  backtest  │
  │persistent│     │  (TUI    │ │  monitor │    │  + sweep   │
  │  (CLI)   │     │ ratatui) │ │ (systemd)│    │  (CLI)     │
  └──────────┘     └──────────┘ └─────┬────┘    └─────┬──────┘
                                      │               │
                                      ▼               ▼
                              ┌──────────────┐  ┌────────────┐
                              │ Telegram Bot │  │  comfy-    │
                              │     API      │  │  table     │
                              └──────────────┘  │  reports   │
                                                └────────────┘
```

## Tech Stack

| Layer | Choice | Why |
|---|---|---|
| Language | Rust 1.95 (edition 2024) | Single-binary deployment, no GC pauses for the polling loop, strict typing on financial math |
| Async runtime | tokio (full features) | One runtime for HTTP, signals, and timers |
| HTTP | reqwest 0.12 (json) | Standard, well-maintained, handles connection pooling |
| Storage | sqlx 0.8 + SQLite | Embedded, zero-ops, sufficient throughput at this scale |
| TUI | ratatui 0.28 + crossterm 0.28 | Modern fork of tui-rs, active development, clean Frame API |
| CLI | clap 4 (derive) | Subcommand structure mirrors the experimental workflow |
| Tables | comfy-table 7 | Terminal output for backtest reports |
| Time | chrono 0.4 | RFC3339 logging + formatted timestamps in the UI |
| Service mgmt | systemd | Linux-native; `KillSignal=SIGINT` matches our app's signal handler |

## Strategy Hypothesis

> Open a position when annualized funding APR exceeds threshold X, hold for up to N hours, exit on either signal loss or `max_hold`. Choose the side that *receives* funding (LONG when funding is negative, SHORT when positive).

Sweep axes (Cartesian product, 216 main + 12 hysteresis sub-sweep = **228 total runs**):

| Axis | Values |
|---|---|
| Entry order type | maker, taker |
| Min funding APR threshold | 50%, 100%, 150%, 200% |
| Hold horizon | 6h, 12h, 24h |
| Max concurrent positions | 1, 3, 5 |
| Exit strategy | `std` (signal-based), `max-hold-only`, `hysteresis` (entry/exit thresholds) |

Cost model:

- **Taker entry**: `spread/2 + taker_fee + slippage` = 10 + 4.5 + 10 = 24.5 bps
- **Maker entry** (mid-fill assumed): `maker_fee` = 1.5 bps
- **Exit** (always taker, to guarantee fill on `signal_lost`): 24.5 bps
- **Round-trip**: 49 bps taker / 26 bps maker

Position sizing in APR-filter mode is fixed at 10% of capital per position (so sweep axes stay independent).

## Why It Doesn't Work

The strategy fails because of a **temporal asymmetry** between funding-rate observation and funding-rate accrual.

1. **Spikes are transient.** A funding APR reading of +400% reflects a single hour's funding rate annualized. By construction, the market mechanism that pays funding *is* the rebalancing force — high funding attracts the opposite side, which collapses funding back toward the cross-venue mean. Within 1–6 hours of a spike, the rate typically reverts 50–80%.

2. **Tighter holds don't help.** Cutting `hold_hours` to 6 means even less time to amortize the round-trip cost. Best 6h combo: ~$0.07 gross funding vs $0.26 maker round-trip cost.

3. **Wider holds don't help.** Extending to 24–48 hours collects mean-reverted (i.e., low) funding for most of the holding period. Average realized funding APR across the verbose best run was 11%, even though the entry filter was 200%+.

4. **The std-based `signal_lost` exit was the leading suspect.** Removing it (the `max-hold-only` exit strategy) marginally improved the best result (−$0.26 vs −$0.63) but did not flip the sign on any combination. The improvement is structural-floor, not a real edge.

5. **Hysteresis (wide entry/exit threshold gap) does not help.** Tested 6 (entry, exit) threshold pairs across hold horizons: all 84 hysteresis combinations are net-negative.

The net-positive parameter sub-region simply does not exist in this surface for this asset universe (top 50 perps by current Hyperliquid volume) over the 30-day window.

## What Wasn't Tested (Limitations of This Study)

- **Cash-and-carry / basis trade.** Long spot + short perp delta-hedges price risk, allowing multi-week holds while collecting funding. Hyperliquid has spot markets; this is the canonical institutional approach to funding farming. It was not implemented because it requires a second order book, separate execution paths, and inventory tracking.
- **Predictive entry signals.** This study uses a *reactive* trigger (funding APR has already crossed the threshold). Predictive signals derived from order-book imbalance, OI velocity, or social sentiment may have better entry timing — at the cost of strategy complexity.
- **Cross-exchange funding arbitrage.** Funding rates differ across Hyperliquid / Bybit / Binance for the same underlying. A multi-venue setup can extract the spread, but adds custodial, network, and synchronization complexity.
- **Sub-account or VIP fee tiers.** A maker fee of 0 bps lowers the maker break-even from 95% to ~50% APR — within reach of average funding levels. VIP requires sustained $10M+ monthly volume, which is not a $1k-account scenario.
- **Out-of-sample validation.** 30 days is a small statistical window. A 90+ day backtest would tighten the confidence interval, but is unlikely to flip the sign given the magnitude of the shortfall (cost-to-gross ratio > 5× in the best case).

## Project Structure

```
funding-scanner/
├── src/
│   ├── main.rs           # CLI dispatch (clap subcommands)
│   ├── util.rs           # Shared formatters (format_usd, format_bytes)
│   ├── watch.rs          # Live TUI dashboard (5 sections, auto-refresh)
│   ├── alerts.rs         # Telegram alerter with rate limiting + flip-flop mute
│   └── backtest.rs       # Backtest engine + 228-combination parameter sweep
├── systemd/
│   ├── funding-collector.service   # Long-running data collector
│   └── funding-alerts.service      # Telegram alert daemon (env-injected secrets)
├── data/                 # SQLite databases (gitignored)
│   └── snapshots.db
├── Cargo.toml
├── Cargo.lock
└── README.md
```

## How to Run

### Prerequisites

- Rust 1.75+ (developed against 1.95)
- Linux for systemd integration; the binary itself runs on macOS without service management
- SQLite (bundled via `libsqlite3-sys`, no system install needed)
- Outbound network access to `api.hyperliquid.xyz` and `api.telegram.org` (for alerts)

### Build

```bash
cargo build --release
```

The optimized binary lands at `target/release/funding-scanner`.

### Commands

```bash
# One-off snapshot of the current market — top 20 perps by |funding APR|
./target/release/funding-scanner scan

# Continuous data collection (default 5-minute interval)
./target/release/funding-scanner collect
./target/release/funding-scanner collect --interval-seconds 60

# Live TUI dashboard (read-only on the SQLite database)
./target/release/funding-scanner watch

# Persistence query (requires >= 6h of collected data for default thresholds)
./target/release/funding-scanner persistent --hours 6 --min-apr 30

# Telegram alerts (requires TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID env vars;
# without them, would-send messages are logged to stdout instead of crashing)
TELEGRAM_BOT_TOKEN=... TELEGRAM_CHAT_ID=... \
  ./target/release/funding-scanner alert-monitor

# Single backtest with default parameters
./target/release/funding-scanner backtest --days 30 --capital 1000

# Backtest with all cost knobs and exit strategies exposed
./target/release/funding-scanner backtest \
  --days 30 --capital 1000 --max-positions 3 \
  --spread-bps 20 --taker-fee-bps 4.5 --maker-fee-bps 1.5 --slippage-bps 10 \
  --use-maker --hold-hours 24 \
  --exit-strategy hysteresis --hysteresis-entry-apr 200 --hysteresis-exit-apr 50

# Full 228-combination parameter sweep + auto verbose run on best combo
./target/release/funding-scanner backtest-sweep --days 30
```

### Production Deploy (systemd)

The repo ships unit files in `systemd/` with placeholder secrets. Real credentials must be injected via `systemctl edit` drop-ins so they never enter version control.

```bash
# Install both unit files
sudo cp systemd/funding-collector.service /etc/systemd/system/
sudo cp systemd/funding-alerts.service    /etc/systemd/system/

# Inject Telegram secrets via drop-in override (creates a separate file under
# /etc/systemd/system/funding-alerts.service.d/ that overrides Environment=)
sudo systemctl edit funding-alerts.service
# In the editor:
# [Service]
# Environment="TELEGRAM_BOT_TOKEN=<real_token>"
# Environment="TELEGRAM_CHAT_ID=<real_chat_id>"

sudo systemctl daemon-reload
sudo systemctl enable --now funding-collector funding-alerts

# Live logs
journalctl -u funding-collector -f
journalctl -u funding-alerts    -f
```

Both services use `KillSignal=SIGINT` because the binary's shutdown handler listens on SIGINT, not SIGTERM. Without this override, `systemctl stop` would trigger SIGKILL after the timeout and be misclassified as a crash by `Restart=on-failure`.

## Author

**[YOUR_USERNAME]** — sole author and developer of this project.

- Designed the research methodology and formulated the hypotheses
- Architected and implemented the entire Rust codebase (collector, TUI, alerter, backtest engine, parameter sweep)
- Selected the parameter axes for the sweep and chose the cost model parameters consistent with current Hyperliquid retail conditions
- Validated and interpreted all results
- Configured the production deployment (systemd units, signal handling, secret injection, sandboxing)

The negative result — falsifying the hypothesis that this funding-farming strategy can survive realistic transaction costs — is the meaningful research output of this study. A robust negative result is more valuable than an unfalsified positive one, because it rules out an entire region of strategy space rather than leaving it ambiguous.

## License

MIT

## Disclaimer

This is research code, not financial advice. The conclusion of this study is that the strategy as specified is **not profitable** at retail-account transaction costs on Hyperliquid. Do not deploy this code with real capital expecting profits.

Past funding rates are not predictive of future funding rates. Real execution will incur additional costs not modeled here (liquidation risk, mark-price drift during the holding period, partial fills on maker entries, exchange downtime, on-chain settlement variance). The backtest assumes instantaneous execution at observed mark prices, which is an upper bound on achievable performance.
