# JetX Data Scraper — Rust Edition

Real-time WebSocket scraper for JetX (crash game on Betway).
Parses all message types, detects data leaks, and prints per-round reports.

---

## Features

| Feature | Detail |
|---|---|
| All 4 message cases | Case 1 (fly), Case 2 (cashout), Case 3 (crash), Case 4 (bet) |
| Counter-aware parsing | Assigns individual hex counters to batched sub-messages |
| Leak detection | Bets during flight, cashouts after crash — flagged separately |
| Round report | Normal vs leaked stats, highest bet, highest cashout |
| Prediction | Q1–Q3 range, mean, median, % below 2x/3x (needs ≥3 rounds) |
| Auto-reconnect | Reconnects on disconnect with 3s delay |
| Colored output | Green=cashout, Blue=bet, Red=crash/leak, Cyan=flying |

---

## Prerequisites

```bash
# Install Rust (https://rustup.rs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

---

## Build

```bash
cd jetx_scraper
cargo build --release
```

The binary is at `target/release/jetx_scraper`.

---

## Run

### Live mode (connect to WebSocket)
```bash
# Uses the hardcoded URL in main.rs — update the token first!
cargo run --release

# Or pass a custom URL:
cargo run --release -- "wss://eu-server-w16.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&token=YOUR_FRESH_TOKEN&..."
```

> **Token refresh**: The `token=` and `connectionToken=` parameters expire.
> To get a fresh one, open Betway in Chrome → DevTools → Network → WS tab →
> click the JetX WebSocket connection → copy the full URL.

### Replay/test mode (parse a file)

You can replay captured messages from `test_messages.txt` by piping them through `stdin` — or add a replay subcommand. For now, to test offline:

```bash
# Quick offline test — pipe test messages
cat test_messages.txt | grep '^{' | while read line; do
  echo "$line"
done
# Then paste into a small Rust test (see src/tests below)
```

---

## Output key

```
[CASE 1 START]  Plane lifted off
[FLYING]        In-flight multiplier updates (overwrite same line)
[CASE 3 CRASH]  Plane crashed — triggers round report
[BET]           Normal bet placed (before takeoff)
[CASHOUT]       Normal cashout (during flight)
[LEAK-BET]      Bet placed after takeoff (late/leaked)
[LEAK-CASHOUT]  Cashout after crash (leaked)
[gBOARD]        Round reset message
[GAP]           Counter gap detected (informational)
```

---

## Prediction model

After each crash the following statistics are computed over all historical rounds:

- **Mean crash** — average multiplier
- **Median** — 50th percentile
- **Q1–Q3 (safe range)** — 25th to 75th percentile, this is the range where 50% of rounds land
- **% below 2x** and **% below 3x** — low-multiplier frequency

The Q1–Q3 range is the conservative "safe zone" to cash out before the median crash.

---

## Project structure

```
jetx_scraper/
├── Cargo.toml          # dependencies
├── README.md           # this file
├── test_messages.txt   # captured sample messages for offline testing
└── src/
    └── main.rs         # all logic: parser, state machine, printer, ws client
```

---

## Customising

- To change the WebSocket URL permanently, edit `WS_URL` in `src/main.rs`
- To increase prediction history: change `200` in `AppState::new()`
- To save data to CSV: add `csv = "1"` to `[dependencies]` and write after each round report
