// =============================================================================
// JetX Data Scraper — Single-Session Edition
//
// KEY DESIGN DECISIONS (learned from the working reference code):
//
//  ✓ NO auto-reconnect  — you redeploy manually so you can inspect the gap
//  ✓ HTTP health server — cloud platforms (Koyeb/Render/Railway/Fly.io) kill
//                         any process that doesn't bind a port; this keeps the
//                         instance alive via cron-job.org / UptimeRobot pings
//  ✓ Permissive TLS    — danger_accept_invalid_certs(true) EXACTLY like the
//                         reference that was working; strict TLS silently drops
//                         the connection and looks like a network failure
//  ✓ 30 s silence watchdog — server can appear "connected" but deliver nothing;
//                         timeout triggers a clean shutdown (you see it in logs)
//  ✓ WS in spawned task — if the WS task dies the HTTP server stays up, so the
//                         platform doesn't kill the whole instance; /health will
//                         report ws_connected: false so you know to redeploy
//  ✓ History persisted  — jetx_history.json written after every round so a
//                         redeploy loses no prediction data
//  ✓ Shutdown timestamp — written to disk so you can find the exact gap
// =============================================================================

use actix_web::{get, App, HttpResponse, HttpServer, Responder};
use colored::*;
use futures_util::{SinkExt, StreamExt};
use native_tls::TlsConnector as NativeTlsConnector;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::protocol::Message, Connector};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// If no WS message arrives for this many seconds, treat the connection as dead
/// and shut the WS task down cleanly (HTTP stays alive, /health reports false).
const SILENCE_TIMEOUT_SECS: u64 = 30;

/// Send a WebSocket Ping frame this often to keep the connection alive.
/// This prevents the server from dropping a silent connection between game rounds.
const WS_PING_INTERVAL_SECS: u64 = 20;

const HISTORY_FILE:   &str = "jetx_history.json";
const SHUTDOWN_FILE:  &str = "jetx_last_shutdown.txt";

// ── Shared state (read by HTTP endpoints) ─────────────────────────────────────

static ROUND_COUNT:    AtomicU32 = AtomicU32::new(0);
static HISTORY_LEN:    AtomicU32 = AtomicU32::new(0);
static WS_CONNECTED:   AtomicBool = AtomicBool::new(false);
/// True once the WS task has exited (either cleanly or via error).
/// /health exposes this so you know exactly when to redeploy.
static WS_DEAD:        AtomicBool = AtomicBool::new(false);
static MSG_COUNT:      AtomicU32 = AtomicU32::new(0);
/// Counts outgoing WS ping frames — visible at /health so you can confirm
/// the heartbeat is actually running.
static PING_COUNT:     AtomicU32 = AtomicU32::new(0);

// ── HTTP Health Endpoints ─────────────────────────────────────────────────────
//
// Point cron-job.org / UptimeRobot at GET /health every 5 minutes.
// The cloud platform sees an open HTTP port and never idles the instance.
// When WS dies you will see  ws_dead: true  here — that's your redeploy signal.

#[get("/")]
async fn root() -> impl Responder {
    HttpResponse::Ok().content_type("application/json").json(json!({
        "service":       "JetX Data Scraper",
        "status":        if WS_DEAD.load(Ordering::Relaxed) { "ws_stopped" } else { "running" },
        "ws_connected":  WS_CONNECTED.load(Ordering::Relaxed),
        "ws_dead":       WS_DEAD.load(Ordering::Relaxed),
        "rounds_this_session": ROUND_COUNT.load(Ordering::Relaxed),
        "history_rounds": HISTORY_LEN.load(Ordering::Relaxed),
        "messages_received": MSG_COUNT.load(Ordering::Relaxed),
        "ws_pings_sent": PING_COUNT.load(Ordering::Relaxed),
        "action": if WS_DEAD.load(Ordering::Relaxed) {
            "redeploy needed — check jetx_last_shutdown.txt for gap info"
        } else {
            "monitoring"
        }
    }))
}

#[get("/health")]
async fn health() -> impl Responder {
    let dead = WS_DEAD.load(Ordering::Relaxed);
    // Return HTTP 200 always so the platform doesn't restart us;
    // but include ws_dead in the body so YOU can see it.
    HttpResponse::Ok().content_type("application/json").json(json!({
        "status":       if dead { "ws_stopped" } else { "healthy" },
        "ws_connected": WS_CONNECTED.load(Ordering::Relaxed),
        "ws_dead":      dead,
        "rounds":       ROUND_COUNT.load(Ordering::Relaxed),
        "messages":     MSG_COUNT.load(Ordering::Relaxed),
        "ws_pings_sent": PING_COUNT.load(Ordering::Relaxed),
    }))
}

#[get("/status")]
async fn status() -> impl Responder {
    HttpResponse::Ok().content_type("application/json").json(json!({
        "ws_connected":  WS_CONNECTED.load(Ordering::Relaxed),
        "ws_dead":       WS_DEAD.load(Ordering::Relaxed),
        "rounds":        ROUND_COUNT.load(Ordering::Relaxed),
        "history_size":  HISTORY_LEN.load(Ordering::Relaxed),
        "messages":      MSG_COUNT.load(Ordering::Relaxed),
        "last_shutdown": std::fs::read_to_string(SHUTDOWN_FILE).unwrap_or_else(|_| "none".into()),
    }))
}

// ── Data Structures ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CashoutEvent {
    counter:     String,   // hex  e.g. "67B0C3F"
    counter_dec: u64,      // decimal e.g. 108986431
    player_name: String,
    bet_usd:     f64,
    bet_local:   f64,
    multiplier:  f64,
    cashout_usd: f64,
    currency:    String,
    slot:        u8,
    player_id:   String,
}

#[derive(Debug, Clone)]
struct BetEvent {
    counter:     String,   // hex
    counter_dec: u64,      // decimal
    player_name: String,
    bet_usd:     f64,
    bet_local:   f64,
    currency:    String,
    slot:        u8,
    player_id:   String,
}

#[derive(Debug, Clone)]
struct RoundResult {
    crash_multiplier: f64,
    flight_time:      f64,
    normal_bets:      Vec<BetEvent>,
    normal_cashouts:  Vec<CashoutEvent>,
    leaked_bets:      Vec<BetEvent>,
    leaked_cashouts:  Vec<CashoutEvent>,
}

// ── Game State Machine ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone)]
enum GameState { WaitingForBets, Flying, Crashed }

struct AppState {
    game_state:      GameState,
    round_number:    u32,
    last_counter:    Option<u64>,
    current_mult:    f64,
    current_time:    f64,
    normal_bets:     Vec<BetEvent>,
    normal_cashouts: Vec<CashoutEvent>,
    leaked_bets:     Vec<BetEvent>,
    leaked_cashouts: Vec<CashoutEvent>,
    history:         VecDeque<f64>,
}

impl AppState {
    fn new() -> Self {
        let history = load_history();
        HISTORY_LEN.store(history.len() as u32, Ordering::Relaxed);
        Self {
            game_state:      GameState::WaitingForBets,
            round_number:    0,
            last_counter:    None,
            current_mult:    1.0,
            current_time:    0.0,
            normal_bets:     Vec::new(),
            normal_cashouts: Vec::new(),
            leaked_bets:     Vec::new(),
            leaked_cashouts: Vec::new(),
            history,
        }
    }

    fn reset_round(&mut self) {
        self.round_number   += 1;
        self.game_state      = GameState::WaitingForBets;
        self.current_mult    = 1.0;
        self.current_time    = 0.0;
        self.normal_bets     .clear();
        self.normal_cashouts .clear();
        self.leaked_bets     .clear();
        self.leaked_cashouts .clear();
        ROUND_COUNT.store(self.round_number, Ordering::Relaxed);
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

fn load_history() -> VecDeque<f64> {
    match std::fs::read_to_string(HISTORY_FILE) {
        Ok(s) => {
            let v: Vec<f64> = serde_json::from_str(&s).unwrap_or_default();
            println!(
                "  {} Loaded {} historical rounds from {}",
                "[HISTORY]".bright_cyan(), v.len(), HISTORY_FILE
            );
            v.into()
        }
        Err(_) => {
            println!("  {} No history file found — starting fresh", "[HISTORY]".yellow());
            VecDeque::with_capacity(200)
        }
    }
}

fn save_history(history: &VecDeque<f64>) {
    let v: Vec<f64> = history.iter().cloned().collect();
    if let Ok(s) = serde_json::to_string(&v) {
        if std::fs::write(HISTORY_FILE, &s).is_ok() {
            HISTORY_LEN.store(v.len() as u32, Ordering::Relaxed);
        }
    }
}

/// Write a shutdown record so you can find the exact data gap on redeploy.
fn write_shutdown_record(reason: &str, rounds: u32) {
    // chrono is not in this Cargo.toml — use a plain Unix timestamp via std
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let record = format!(
        "shutdown_unix_ts={}\nreason={}\nrounds_this_session={}\n",
        ts, reason, rounds
    );
    let _ = std::fs::write(SHUTDOWN_FILE, &record);
    println!(
        "  {} Shutdown record written to {} (ts={}, rounds={})",
        "[SHUTDOWN]".yellow(), SHUTDOWN_FILE, ts, rounds
    );
}

// ── Counter helpers ───────────────────────────────────────────────────────────

fn parse_counter(c_field: &str) -> Option<(String, u64)> {
    let hex_str = c_field.split(',').nth(1)?.split('|').next()?;
    let val     = u64::from_str_radix(hex_str.trim(), 16).ok()?;
    Some((hex_str.trim().to_string(), val))
}

fn assign_sub_counters(current_val: u64, count: usize) -> Vec<String> {
    let base = current_val.saturating_sub(count as u64 - 1);
    (0..count).map(|i| format!("{:X}", base + i as u64)).collect()
}

// ── Event parsers ─────────────────────────────────────────────────────────────

fn parse_cashout(a: &str, counter: &str) -> Option<CashoutEvent> {
    let p: Vec<&str> = a.split('_').collect();
    if p.len() < 9 { return None; }
    let counter_dec = u64::from_str_radix(counter, 16).unwrap_or(0);
    Some(CashoutEvent {
        counter:     counter.to_string(),
        counter_dec,
        player_name: p[0].to_string(),
        bet_usd:     p[1].parse().ok()?,
        bet_local:   p[2].parse().ok()?,
        multiplier:  p[3].parse().ok()?,
        cashout_usd: p[4].parse().ok()?,
        player_id:   p[5].to_string(),
        currency:    p[7].to_string(),
        slot:        p[8].parse().unwrap_or(0),
    })
}

fn parse_bet(a: &str, counter: &str) -> Option<BetEvent> {
    let p: Vec<&str> = a.split('_').collect();
    if p.len() < 9 { return None; }
    let counter_dec = u64::from_str_radix(counter, 16).unwrap_or(0);
    Some(BetEvent {
        counter:     counter.to_string(),
        counter_dec,
        player_name: p[0].to_string(),
        bet_usd:     p[1].parse().ok()?,
        bet_local:   p[2].parse().ok()?,
        player_id:   p[5].to_string(),
        currency:    p[7].to_string(),
        slot:        p[8].parse().unwrap_or(0),
    })
}

// ── Printers ─────────────────────────────────────────────────────────────────

fn separator(label: &str) {
    println!(
        "\n{}",
        format!("──── {} {}", label, "─".repeat(60usize.saturating_sub(label.len() + 6)))
            .bright_black()
    );
}

fn print_cashout(ev: &CashoutEvent, is_leak: bool) {
    let tag = if is_leak { "[LEAK-CASHOUT]".red().bold() } else { "[CASHOUT]     ".green().bold() };
    println!(
        "  {} {:>8} | Ctr: {} / {} | Bet:${:.6} ({:.2}{}) | @{:.2}x→${:.6} | Slot:{} | ID:{}",
        tag, ev.player_name.cyan(),
        ev.counter.yellow(),                          // HEX
        ev.counter_dec.to_string().bright_white(),    // DECIMAL
        ev.bet_usd, ev.bet_local, ev.currency,
        ev.multiplier, ev.cashout_usd, ev.slot, ev.player_id.bright_black()
    );
}

fn print_bet(ev: &BetEvent, is_leak: bool) {
    let tag = if is_leak { "[LEAK-BET]    ".magenta().bold() } else { "[BET]         ".blue().bold() };
    println!(
        "  {} {:>8} | Ctr: {} / {} | Bet:${:.6} ({:.2}{}) | Slot:{} | ID:{}",
        tag, ev.player_name.cyan(),
        ev.counter.yellow(),                          // HEX
        ev.counter_dec.to_string().bright_white(),    // DECIMAL
        ev.bet_usd, ev.bet_local, ev.currency,
        ev.slot, ev.player_id.bright_black()
    );
}

fn print_flight(mult: f64, time: f64, is_start: bool) {
    use std::io::Write;
    if is_start {
        println!(
            "  {} Plane lifted off — {:.2}x  {:.2}s",
            "[CASE 1 START]".bright_cyan().bold(), mult, time
        );
    } else {
        print!("\r  {} {:.3}x  {:.2}s    ", "[FLYING]".bright_cyan(), mult, time);
        let _ = std::io::stdout().flush();
    }
}

fn print_crash(mult: f64, time: f64) {
    println!(
        "\n  {} CRASHED at {:.2}x after {:.2}s",
        "[CASE 3 CRASH]".red().bold(), mult, time
    );
}

fn print_board_reset(round: u32) {
    println!("  {} New round board reset → Round {}", "[gBOARD]".bright_black().bold(), round);
}

fn print_round_report(result: &RoundResult, history: &VecDeque<f64>) {
    println!("\n{}", "═".repeat(72).yellow());
    println!("{}", "  ROUND REPORT".yellow().bold());
    println!("{}", "═".repeat(72).yellow());

    println!("  Multiplier at crash : {:.2}x", result.crash_multiplier);
    println!("  Time of flight      : {:.2}s", result.flight_time);

    let n_bets:  f64 = result.normal_bets    .iter().map(|b| b.bet_usd    ).sum();
    let n_cos:   f64 = result.normal_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("\n  {} Normal Data", "▶".green());
    println!("    Players that placed bets  : {}", result.normal_bets    .len().to_string().green());
    println!("    Players that cashed out   : {}", result.normal_cashouts.len().to_string().green());
    println!("    Total bets (USD)          : ${:.6}", n_bets);
    println!("    Total cashouts (USD)      : ${:.6}", n_cos);

    let l_bets:  f64 = result.leaked_bets    .iter().map(|b| b.bet_usd    ).sum();
    let l_cos:   f64 = result.leaked_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("\n  {} Leaked Data", "▶".red());
    println!("    Leaked bet players        : {}", result.leaked_bets    .len().to_string().red());
    println!("    Leaked cashout players    : {}", result.leaked_cashouts.len().to_string().red());
    println!("    Leaked bets (USD)         : ${:.6}", l_bets);
    println!("    Leaked cashouts (USD)     : ${:.6}", l_cos);

    println!("\n  {} Sum Total", "▶".bright_white());
    println!("    Sum players bet           : {}", result.normal_bets.len() + result.leaked_bets.len());
    println!("    Sum players cashed out    : {}", result.normal_cashouts.len() + result.leaked_cashouts.len());
    println!("    Sum bets (USD)            : ${:.6}", n_bets + l_bets);
    println!("    Sum cashouts (USD)        : ${:.6}", n_cos + l_cos);

    println!("\n  {} Highlights", "▶".bright_yellow());
    if let Some(t) = result.normal_bets.iter().max_by(|a,b| a.bet_usd.partial_cmp(&b.bet_usd).unwrap()) {
        println!("    Highest bettor   : {} — ${:.6} ({:.2} {})", t.player_name.cyan(), t.bet_usd, t.bet_local, t.currency);
    }
    if let Some(t) = result.normal_cashouts.iter().max_by(|a,b| a.cashout_usd.partial_cmp(&b.cashout_usd).unwrap()) {
        println!("    Highest cashout  : {} — ${:.6} (bet ${:.6} @{:.2}x)", t.player_name.cyan(), t.cashout_usd, t.bet_usd, t.multiplier);
    }

    if history.len() >= 3 {
        let n = history.len() as f64;
        let mean: f64 = history.iter().sum::<f64>() / n;
        let mut sorted: Vec<f64> = history.iter().cloned().collect();
        sorted.sort_by(|a,b| a.partial_cmp(b).unwrap());
        let q1  = sorted[(n * 0.25) as usize];
        let med = sorted[(n * 0.50) as usize];
        let q3  = sorted[(n * 0.75) as usize];
        let p2  = history.iter().filter(|&&v| v < 2.0).count() as f64 / n * 100.0;
        let p3  = history.iter().filter(|&&v| v < 3.0).count() as f64 / n * 100.0;
        println!("\n  {} Prediction (n={})", "▶".bright_cyan(), history.len());
        println!("    Mean crash              : {:.2}x", mean);
        println!("    Median                  : {:.2}x", med);
        println!("    Q1 – Q3 (safe range)    : {:.2}x – {:.2}x", q1, q3);
        println!("    Crashed below 2x        : {:.1}%", p2);
        println!("    Crashed below 3x        : {:.1}%", p3);
        let hist_str: Vec<String> = history.iter().rev().take(20).map(|v| {
            let s = format!("{:.2}x", v);
            if *v < 2.0 { s.red().to_string() } else if *v < 5.0 { s.yellow().to_string() } else { s.green().to_string() }
        }).collect();
        println!("    Last {}           : {}", history.len().min(20), hist_str.join("  "));
    } else {
        println!("\n  {} Prediction: need ≥3 rounds (have {})", "▶".bright_cyan(), history.len());
    }

    println!("{}\n", "═".repeat(72).yellow());
}

// ── Core message processor ────────────────────────────────────────────────────

fn process_message(raw: &str, state: &mut AppState) {
    let json: Value = match serde_json::from_str(raw) {
        Ok(v)  => v,
        Err(_) => return,
    };

    let c_field  = json["C"].as_str().unwrap_or("");
    let messages = match json["M"].as_array() {
        Some(a) => a,
        None    => return,
    };
    if messages.is_empty() { return; }

    MSG_COUNT.fetch_add(1, Ordering::Relaxed);

    let (counter_str, counter_val) = parse_counter(c_field)
        .unwrap_or_else(|| ("UNK".to_string(), 0));

    let sub_ctrs: Vec<String> = if messages.len() > 1 {
        assign_sub_counters(counter_val, messages.len())
    } else {
        vec![counter_str.clone()]
    };

    if let Some(last) = state.last_counter {
        if counter_val > last + 1 && messages.len() == 1 {
            println!(
                "  {} Counter gap: {} → {} (Δ{})",
                "[GAP]".bright_black(),
                format!("{:X}", last).yellow(),
                counter_str.yellow(),
                counter_val - last - 1
            );
        }
    }
    if counter_val > 0 { state.last_counter = Some(counter_val); }

    if messages.len() > 1 {
        separator(&format!("BATCH x{} @ {}", messages.len(), counter_str));
    }

    let crash_idx: Option<usize> = messages.iter().position(|m| {
        m["M"].as_str() == Some("response") && m["A"][0]["f"].as_bool() == Some(true)
    });
    let mut crashed_in_batch = false;

    for (idx, msg) in messages.iter().enumerate() {
        let ctr = sub_ctrs.get(idx).cloned().unwrap_or_else(|| counter_str.clone());

        match msg["M"].as_str().unwrap_or("") {

            "gBoard" => {
                state.reset_round();
                print_board_reset(state.round_number);
            }

            "response" => {
                let a = &msg["A"][0];
                let f = a["f"].as_bool().unwrap_or(false);
                let v = a["v"].as_f64().unwrap_or(1.0);
                let s = a["s"].as_f64().unwrap_or(0.0);

                if !f {
                    state.current_mult = v;
                    state.current_time = s;
                    if v == 1.0 && s == 0.0 {
                        state.game_state = GameState::Flying;
                        print_flight(v, s, true);
                    } else {
                        print_flight(v, s, false);
                    }
                } else {
                    state.game_state  = GameState::Crashed;
                    crashed_in_batch  = true;
                    print_crash(v, s);

                    let result = RoundResult {
                        crash_multiplier: v,
                        flight_time:      s,
                        normal_bets:      state.normal_bets    .clone(),
                        normal_cashouts:  state.normal_cashouts .clone(),
                        leaked_bets:      state.leaked_bets     .clone(),
                        leaked_cashouts:  state.leaked_cashouts .clone(),
                    };

                    state.history.push_back(v);
                    if state.history.len() > 500 { state.history.pop_front(); }
                    save_history(&state.history);

                    print_round_report(&result, &state.history);
                }
            }

            "g" => {
                let inner_m = msg["A"][0]["M"].as_str().unwrap_or("");
                let a_str   = msg["A"][0]["I"]["a"].as_str().unwrap_or("");
                let after   = crashed_in_batch || crash_idx.map(|ci| idx > ci).unwrap_or(false);

                match inner_m {
                    "c" => match parse_cashout(a_str, &ctr) {
                        Some(ev) => {
                            let leak = state.game_state == GameState::Crashed || after;
                            print_cashout(&ev, leak);
                            if leak { state.leaked_cashouts.push(ev); } else { state.normal_cashouts.push(ev); }
                        }
                        None => println!("  {} Bad cashout: {}", "[WARN]".yellow(), a_str),
                    },
                    "b" => match parse_bet(a_str, &ctr) {
                        Some(ev) => {
                            let leak = matches!(state.game_state, GameState::Flying | GameState::Crashed);
                            print_bet(&ev, leak);
                            if leak { state.leaked_bets.push(ev); } else { state.normal_bets.push(ev); }
                        }
                        None => println!("  {} Bad bet: {}", "[WARN]".yellow(), a_str),
                    },
                    other => println!("  {} Unknown g sub '{}' @ {}", "[UNK]".bright_black(), other, ctr),
                }
            }

            other if !other.is_empty() =>
                println!("  {} Unknown msg '{}' @ {}", "[UNK]".bright_black(), other, ctr),
            _ => {}
        }
    }
}

// ── Single WebSocket session — NO reconnect, NO negotiate, NO env vars ────────
//
// HOW TO UPDATE THE URL WHEN YOU GET A 404:
//   1. Open Betway JetX in Chrome
//   2. DevTools → Network → WS tab → click the connection → copy the full URL
//   3. Paste it below as WS_URL, replacing the old one
//   4. Redeploy
//
// That's it. No env vars. No negotiate. Just paste and redeploy.

// !! PASTE YOUR FULL WEBSOCKET URL HERE — replace on every redeploy !!
const WS_URL: &str = "wss://eu-server-w16.ssgportal.com/JetXNode703/signalr/connect?transport=webSockets&clientProtocol=1.5&token=9e5f4620-b2c5-46af-87aa-5a78e9ba2f7f&group=JetX&connectionToken=VQqiTFoOoN3%2FFoBWqHR9PNMwOqayvu3f7YK5iM6MHzRg7uLnUUuzgV6RyYIFUhDjOhfuwKkV70NzWyIUKOIykruEm67qQLsKOJ94WY3vkCeO4632ZWklT6ErCgA2I2Qh&connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D&tid=0";

async fn run_ws_session() {
    let mut state = AppState::new();

    println!("  {} Connecting to: {}", "[WS]".bright_yellow(),
        WS_URL.split('?').next().unwrap_or(WS_URL).yellow());

    // Permissive TLS — same as the working reference code.
    let connector = match NativeTlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
    {
        Ok(c)  => c,
        Err(e) => {
            eprintln!("  {} TLS build failed: {}", "[ERROR]".red(), e);
            WS_DEAD.store(true, Ordering::Relaxed);
            write_shutdown_record(&format!("tls_build_failed: {}", e), state.round_number);
            return;
        }
    };
    let connector = Connector::NativeTls(connector);

    let ws_stream = match timeout(
        Duration::from_secs(15),
        connect_async_tls_with_config(WS_URL, None, false, Some(connector))
    ).await {
        Err(_) => {
            eprintln!("  {} Connect timed out (15s)", "[ERROR]".red());
            WS_DEAD.store(true, Ordering::Relaxed);
            write_shutdown_record("connect_timeout_15s", state.round_number);
            return;
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            if msg.contains("404") || msg.contains("403") || msg.contains("401") {
                eprintln!("  {} WS connect error: {}", "[ERROR]".red(), msg);
                eprintln!("  {} The connectionToken in WS_URL has expired.", "[404]".red().bold());
                eprintln!("  {} Open Chrome DevTools → Network → WS tab → copy the full URL → paste into WS_URL in main.rs → redeploy.", "[FIX]".yellow());
            } else {
                eprintln!("  {} WS connect error: {}", "[ERROR]".red(), msg);
            }
            WS_DEAD.store(true, Ordering::Relaxed);
            write_shutdown_record(&format!("ws_connect_error: {}", e), state.round_number);
            return;
        }
        Ok(Ok((stream, _))) => stream,
    };

    WS_CONNECTED.store(true, Ordering::Relaxed);
    println!("  {} Connected", "[OK]".green().bold());

    let (mut write, mut read) = ws_stream.split();

    // SignalR handshake
    if let Err(e) = write.send(Message::Text("{}".into())).await {
        eprintln!("  {} Handshake send failed: {}", "[ERROR]".red(), e);
        WS_CONNECTED.store(false, Ordering::Relaxed);
        WS_DEAD.store(true, Ordering::Relaxed);
        write_shutdown_record(&format!("handshake_failed: {}", e), state.round_number);
        return;
    }

    // ── Heartbeat: send a WS Ping frame every WS_PING_INTERVAL_SECS ────────────
    //
    // WHY THIS IS NEEDED:
    //   Between game rounds the server goes quiet for 10–20 seconds.
    //   Some load balancers / NAT routers drop idle TCP connections after ~30s
    //   of silence.  Sending a Ping frame resets that idle timer on every hop
    //   between us and the server, so the connection stays open indefinitely.
    //
    //   This is separate from the HTTP cron ping — that keeps the cloud instance
    //   alive.  This ping keeps the WebSocket connection itself alive.
    //
    // HOW IT WORKS:
    //   We give the writer half to a background task via an mpsc channel.
    //   The ping task owns the writer and wakes every WS_PING_INTERVAL_SECS.
    //   The reader loop sends Pong frames and regular messages through the same
    //   channel so everything goes through one serialised writer.

    // Channel: reader loop → writer task
    // Messages: either a Pong reply OR a data frame (currently only Pong needed)
    let (tx, mut rx) = mpsc::channel::<Message>(16);

    // Clone tx for the ping timer
    let ping_tx = tx.clone();

    // ── Writer task: owns the write half, drains the channel ─────────────────
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
        // Channel closed → reader loop exited → write half can be dropped
    });

    // ── Ping timer task: fires every WS_PING_INTERVAL_SECS ───────────────────
    tokio::spawn(async move {
        let interval = Duration::from_secs(WS_PING_INTERVAL_SECS);
        loop {
            tokio::time::sleep(interval).await;
            let count = PING_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            println!(
                "  {} Sending WS keepalive ping #{} (every {}s)",
                "[PING]".bright_black(), count, WS_PING_INTERVAL_SECS
            );
            // If the channel is closed (session ended) this will error and the
            // task exits cleanly on next iteration.
            if ping_tx.send(Message::Ping(vec![])).await.is_err() {
                break;
            }
        }
    });

    let silence_dur = Duration::from_secs(SILENCE_TIMEOUT_SECS);
    let session_start = Instant::now();

    println!(
        "  {} Listening… (WS ping every {}s, silence watchdog {}s)",
        "[WS]".bright_cyan(), WS_PING_INTERVAL_SECS, SILENCE_TIMEOUT_SECS
    );

    loop {
        match timeout(silence_dur, read.next()).await {

            // ── 30-second silence → stale connection ──────────────────────────
            // NOTE: a Pong reply from the server resets this timer, so as long
            // as the server responds to our pings this watchdog won't fire.
            Err(_) => {
                let reason = format!(
                    "silence_{}s_after_{}s_session_{}_rounds",
                    SILENCE_TIMEOUT_SECS,
                    session_start.elapsed().as_secs(),
                    state.round_number
                );
                eprintln!(
                    "\n  {} No data for {}s (not even a Pong) — connection is dead.",
                    "[WATCHDOG]".red().bold(), SILENCE_TIMEOUT_SECS
                );
                save_history(&state.history);
                WS_CONNECTED.store(false, Ordering::Relaxed);
                WS_DEAD.store(true, Ordering::Relaxed);
                write_shutdown_record(&reason, state.round_number);
                return;
            }

            // ── Stream ended cleanly ───────────────────────────────────────────
            Ok(None) => {
                let reason = format!(
                    "stream_ended_cleanly_after_{}s_{}_rounds",
                    session_start.elapsed().as_secs(),
                    state.round_number
                );
                println!("\n  {} Stream ended cleanly.", "[WS]".yellow());
                save_history(&state.history);
                WS_CONNECTED.store(false, Ordering::Relaxed);
                WS_DEAD.store(true, Ordering::Relaxed);
                write_shutdown_record(&reason, state.round_number);
                return;
            }

            // ── Read error ────────────────────────────────────────────────────
            Ok(Some(Err(e))) => {
                let reason = format!(
                    "read_error_after_{}s_{}_rounds: {}",
                    session_start.elapsed().as_secs(),
                    state.round_number,
                    e
                );
                eprintln!("\n  {} WS read error: {}", "[ERROR]".red(), e);
                save_history(&state.history);
                WS_CONNECTED.store(false, Ordering::Relaxed);
                WS_DEAD.store(true, Ordering::Relaxed);
                write_shutdown_record(&reason, state.round_number);
                return;
            }

            // ── Normal game data ──────────────────────────────────────────────
            Ok(Some(Ok(Message::Text(text)))) => {
                let t = text.trim();
                if !t.is_empty() && t != "{}" {
                    process_message(t, &mut state);
                }
            }

            // ── Server sent us a Ping → reply with Pong via the writer task ───
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = tx.send(Message::Pong(data)).await;
            }

            // ── Server Pong in response to our keepalive Ping ─────────────────
            Ok(Some(Ok(Message::Pong(_)))) => {
                // Receiving a Pong means the connection is alive.
                // The timeout above is reset just by receiving this frame.
                println!("  {} Pong received — connection confirmed alive", "[PONG]".bright_black());
            }

            // ── Server closed the connection ──────────────────────────────────
            Ok(Some(Ok(Message::Close(frame)))) => {
                let reason = format!(
                    "server_close_after_{}s_{}_rounds: {:?}",
                    session_start.elapsed().as_secs(),
                    state.round_number,
                    frame
                );
                println!("\n  {} Server sent Close frame: {:?}", "[WS]".yellow(), frame);
                save_history(&state.history);
                WS_CONNECTED.store(false, Ordering::Relaxed);
                WS_DEAD.store(true, Ordering::Relaxed);
                write_shutdown_record(&reason, state.round_number);
                return;
            }

            Ok(Some(Ok(_))) => {} // Binary, Frame — ignore
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("{}", "╔══════════════════════════════════════════════════════════╗".bright_cyan());
    println!("{}", "║      JetX Scraper — Single-Session / Manual Redeploy     ║".bright_cyan().bold());
    println!("{}", "╚══════════════════════════════════════════════════════════╝".bright_cyan());
    println!();
    println!("  {} NO auto-reconnect  — redeploy manually to check the gap", "•".yellow());
    println!("  {} HTTP health server — keeps cloud instance alive via cron pings", "•".green());
    println!("  {} WS ping every {}s  — keeps WebSocket connection alive", "•".green(), WS_PING_INTERVAL_SECS);
    println!("  {} {}s silence watchdog — fires only if server stops responding to pings", "•".green(), SILENCE_TIMEOUT_SECS);
    println!("  {} Permissive TLS (danger_accept_invalid_certs=true)", "•".green());
    println!("  {} History saved to {} after every crash", "•".green(), HISTORY_FILE);
    println!("  {} Shutdown record → {}", "•".green(), SHUTDOWN_FILE);
    println!();
    println!("  Two separate keep-alive mechanisms:");
    println!("    1. HTTP cron ping  → keeps the Koyeb instance from sleeping");
    println!("    2. WS Ping frame   → keeps the WebSocket TCP connection open");
    println!();
    println!("  When you get a 404:");
    println!("  1. Chrome DevTools → Network → WS tab → right-click connection → Copy URL");
    println!("  2. Paste it as WS_URL in main.rs (line ~560)");
    println!("  3. Redeploy");
    println!();

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8000".to_string())
        .parse()
        .unwrap_or(8000);

    println!("  HTTP port : {}", port);
    println!("  WS node   : {}", WS_URL.split('/').nth(2).unwrap_or("?").yellow());
    println!();

    tokio::spawn(run_ws_session());

    HttpServer::new(|| {
        App::new()
            .service(root)
            .service(health)
            .service(status)
    })
    .bind(("0.0.0.0", port))?
    .workers(2)
    .run()
    .await
}
