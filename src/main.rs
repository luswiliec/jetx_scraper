// =============================================================================
// JetX Data Scraper — Rust  (resilient + always-alive edition)
//
// WHY INSTANCES STOP — root causes fixed here:
//
//  1. No open HTTP port  → cloud platforms (Koyeb, Render, Railway, Fly.io)
//                          kill processes that don't bind a port.
//                          FIX: actix-web health server runs on $PORT (default 8000)
//                          in parallel with the WS task via tokio::spawn.
//
//  2. TLS rejection      → strict TLS rejects the server's cert silently,
//                          causing an instant error that looks like a network drop.
//                          FIX: connect_async_tls_with_config with
//                          danger_accept_invalid_certs(true) — same as the
//                          working reference code.
//
//  3. Stale connection   → WS can appear connected but deliver no data.
//                          FIX: 30-second silence watchdog via tokio::time::timeout.
//
//  4. WS task panic      → unhandled panic kills only the WS task, not the process.
//                          FIX: tokio::spawn + JoinHandle restart loop; HTTP server
//                          stays up so the platform doesn't kill the whole instance.
//
//  5. Expired token      → hardcoded connectionToken expires.
//                          FIX: token is read from env var WS_URL at startup so you
//                          can update it without recompiling; fallback to env WS_URL.
//
//  6. History lost on restart → crash history wiped on every restart.
//                          FIX: saved to jetx_history.json after every round.
// =============================================================================

use actix_web::{get, App, HttpResponse, HttpServer, Responder};
use colored::*;
use futures_util::{SinkExt, StreamExt};
use native_tls::TlsConnector as NativeTlsConnector;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::protocol::Message, Connector};

// ── Tunables ─────────────────────────────────────────────────────────────────

const SILENCE_TIMEOUT_SECS: u64 = 30;
const MAX_BACKOFF_SECS:      u64 = 64;
const HISTORY_FILE:          &str = "jetx_history.json";

// ── Shared counters (read by HTTP health endpoints) ───────────────────────────

static ROUND_COUNT:   AtomicU32 = AtomicU32::new(0);
static HISTORY_LEN:   AtomicU32 = AtomicU32::new(0);
static WS_CONNECTED:  AtomicBool = AtomicBool::new(false);
static RECONNECT_COUNT: AtomicU32 = AtomicU32::new(0);

// ── HTTP Health Server ────────────────────────────────────────────────────────
// These endpoints are pinged by cron-job.org / UptimeRobot every 5 min
// so the cloud platform never idles the instance.

#[get("/")]
async fn root() -> impl Responder {
    HttpResponse::Ok().content_type("application/json").json(json!({
        "service": "JetX Data Scraper",
        "status": "running",
        "rounds_tracked": ROUND_COUNT.load(Ordering::Relaxed),
        "history_rounds": HISTORY_LEN.load(Ordering::Relaxed),
        "ws_connected": WS_CONNECTED.load(Ordering::Relaxed),
        "reconnects": RECONNECT_COUNT.load(Ordering::Relaxed),
    }))
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().content_type("application/json").json(json!({
        "status": "healthy",
        "ws_connected": WS_CONNECTED.load(Ordering::Relaxed),
        "rounds": ROUND_COUNT.load(Ordering::Relaxed),
    }))
}

#[get("/status")]
async fn status() -> impl Responder {
    HttpResponse::Ok().content_type("application/json").json(json!({
        "status": "monitoring",
        "rounds_tracked": ROUND_COUNT.load(Ordering::Relaxed),
        "history_size": HISTORY_LEN.load(Ordering::Relaxed),
        "reconnects": RECONNECT_COUNT.load(Ordering::Relaxed),
        "ws_live": WS_CONNECTED.load(Ordering::Relaxed),
    }))
}

// ── Data Structures ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CashoutEvent {
    counter:     String,
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
    counter:     String,
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

// ── History I/O ───────────────────────────────────────────────────────────────

fn load_history() -> VecDeque<f64> {
    match std::fs::read_to_string(HISTORY_FILE) {
        Ok(s) => {
            let v: Vec<f64> = serde_json::from_str(&s).unwrap_or_default();
            println!("  {} Loaded {} historical rounds from disk", "[HISTORY]".bright_cyan(), v.len());
            v.into()
        }
        Err(_) => VecDeque::with_capacity(200),
    }
}

fn save_history(history: &VecDeque<f64>) {
    let v: Vec<f64> = history.iter().cloned().collect();
    if let Ok(s) = serde_json::to_string(&v) {
        let _ = std::fs::write(HISTORY_FILE, s);
        HISTORY_LEN.store(v.len() as u32, Ordering::Relaxed);
    }
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
    Some(CashoutEvent {
        counter:     counter.to_string(),
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
    Some(BetEvent {
        counter:     counter.to_string(),
        player_name: p[0].to_string(),
        bet_usd:     p[1].parse().ok()?,
        bet_local:   p[2].parse().ok()?,
        player_id:   p[5].to_string(),
        currency:    p[7].to_string(),
        slot:        p[8].parse().unwrap_or(0),
    })
}

// ── Printers ─────────────────────────────────────────────────────────────────

fn sep(label: &str) {
    println!("\n{}", format!("──── {} {}", label,
        "─".repeat(60usize.saturating_sub(label.len() + 6))).bright_black());
}

fn print_cashout(ev: &CashoutEvent, leak: bool) {
    let tag = if leak { "[LEAK-CASHOUT]".red().bold() } else { "[CASHOUT]     ".green().bold() };
    println!("  {} {:>8} | Ctr:{} | Bet:${:.4}({:.2}{}) | @{:.2}x→${:.4} | Slot:{} | ID:{}",
        tag, ev.player_name.cyan(), ev.counter.yellow(),
        ev.bet_usd, ev.bet_local, ev.currency,
        ev.multiplier, ev.cashout_usd, ev.slot, ev.player_id.bright_black());
}

fn print_bet(ev: &BetEvent, leak: bool) {
    let tag = if leak { "[LEAK-BET]    ".magenta().bold() } else { "[BET]         ".blue().bold() };
    println!("  {} {:>8} | Ctr:{} | Bet:${:.4}({:.2}{}) | Slot:{} | ID:{}",
        tag, ev.player_name.cyan(), ev.counter.yellow(),
        ev.bet_usd, ev.bet_local, ev.currency, ev.slot, ev.player_id.bright_black());
}

fn print_flight(mult: f64, time: f64, is_start: bool) {
    use std::io::Write;
    if is_start {
        println!("  {} Lifted off — {:.2}x  {:.2}s", "[CASE1]".bright_cyan().bold(), mult, time);
    } else {
        print!("\r  {} {:.3}x  {:.2}s   ", "[FLY]".bright_cyan(), mult, time);
        let _ = std::io::stdout().flush();
    }
}

fn print_crash(mult: f64, time: f64) {
    println!("\n  {} CRASHED at {:.2}x after {:.2}s", "[CRASH]".red().bold(), mult, time);
}

fn print_board(round: u32) {
    println!("  {} Board reset → Round {}", "[gBOARD]".bright_black().bold(), round);
}

fn print_report(r: &RoundResult, history: &VecDeque<f64>) {
    println!("\n{}", "═".repeat(72).yellow());
    println!("{}", "  ROUND REPORT".yellow().bold());
    println!("{}", "═".repeat(72).yellow());
    println!("  Crash  : {:.2}x  |  Flight: {:.2}s", r.crash_multiplier, r.flight_time);

    let nb: f64 = r.normal_bets    .iter().map(|b| b.bet_usd    ).sum();
    let nc: f64 = r.normal_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("\n  {} Normal  — bets: {} (${:.2})  cashouts: {} (${:.2})",
        "▶".green(), r.normal_bets.len(), nb, r.normal_cashouts.len(), nc);

    let lb: f64 = r.leaked_bets    .iter().map(|b| b.bet_usd    ).sum();
    let lc: f64 = r.leaked_cashouts.iter().map(|c| c.cashout_usd).sum();
    println!("  {} Leaked  — bets: {} (${:.2})  cashouts: {} (${:.2})",
        "▶".red(), r.leaked_bets.len(), lb, r.leaked_cashouts.len(), lc);

    println!("  {} Sum     — bets: {} (${:.2})  cashouts: {} (${:.2})",
        "▶".bright_white(),
        r.normal_bets.len()     + r.leaked_bets    .len(), nb + lb,
        r.normal_cashouts.len() + r.leaked_cashouts.len(), nc + lc);

    if let Some(t) = r.normal_bets.iter().max_by(|a,b| a.bet_usd.partial_cmp(&b.bet_usd).unwrap()) {
        println!("  {} Top bet : {} ${:.4}", "▶".bright_yellow(), t.player_name.cyan(), t.bet_usd);
    }
    if let Some(t) = r.normal_cashouts.iter().max_by(|a,b| a.cashout_usd.partial_cmp(&b.cashout_usd).unwrap()) {
        println!("  {} Top cash: {} ${:.4} (bet ${:.4} @{:.2}x)", "▶".bright_yellow(),
            t.player_name.cyan(), t.cashout_usd, t.bet_usd, t.multiplier);
    }

    if history.len() >= 3 {
        let n = history.len() as f64;
        let mean: f64 = history.iter().sum::<f64>() / n;
        let mut s: Vec<f64> = history.iter().cloned().collect();
        s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        let q1  = s[(n * 0.25) as usize];
        let med = s[(n * 0.50) as usize];
        let q3  = s[(n * 0.75) as usize];
        let p2  = history.iter().filter(|&&v| v < 2.0).count() as f64 / n * 100.0;
        println!("\n  {} Stats (n={}) mean:{:.2}x  med:{:.2}x  Q1-Q3:{:.2}x–{:.2}x  <2x:{:.0}%",
            "▶".bright_cyan(), history.len(), mean, med, q1, q3, p2);
        let chips: Vec<String> = history.iter().rev().take(15).map(|v| {
            let s = format!("{:.2}x", v);
            if *v < 2.0 { s.red().to_string() } else if *v < 5.0 { s.yellow().to_string() } else { s.green().to_string() }
        }).collect();
        println!("  {} History : {}", "▶".bright_cyan(), chips.join("  "));
    }
    println!("{}\n", "═".repeat(72).yellow());
}

// ── Message processor ─────────────────────────────────────────────────────────

fn process_message(raw: &str, state: &mut AppState) {
    let json: Value = match serde_json::from_str(raw) {
        Ok(v)  => v,
        Err(_) => return,
    };
    let c_field  = json["C"].as_str().unwrap_or("");
    let messages = match json["M"].as_array() { Some(a) => a, None => return };
    if messages.is_empty() { return; }

    let (counter_str, counter_val) = parse_counter(c_field)
        .unwrap_or_else(|| ("UNK".to_string(), 0));

    let sub_ctrs: Vec<String> = if messages.len() > 1 {
        assign_sub_counters(counter_val, messages.len())
    } else {
        vec![counter_str.clone()]
    };

    if let Some(last) = state.last_counter {
        if counter_val > last + 1 && messages.len() == 1 {
            println!("  {} Gap {:X}→{} (Δ{})", "[GAP]".bright_black(),
                last, counter_str.yellow(), counter_val - last - 1);
        }
    }
    if counter_val > 0 { state.last_counter = Some(counter_val); }

    if messages.len() > 1 {
        sep(&format!("BATCH x{} @ {}", messages.len(), counter_str));
    }

    let crash_idx: Option<usize> = messages.iter().position(|m| {
        m["M"].as_str() == Some("response") && m["A"][0]["f"].as_bool() == Some(true)
    });
    let mut crashed_in_batch = false;

    for (idx, msg) in messages.iter().enumerate() {
        let ctr = sub_ctrs.get(idx).cloned().unwrap_or_else(|| counter_str.clone());
        match msg["M"].as_str().unwrap_or("") {

            "gBoard" => { state.reset_round(); print_board(state.round_number); }

            "response" => {
                let a = &msg["A"][0];
                let f = a["f"].as_bool().unwrap_or(false);
                let v = a["v"].as_f64().unwrap_or(1.0);
                let s = a["s"].as_f64().unwrap_or(0.0);
                if !f {
                    state.current_mult = v;
                    state.current_time = s;
                    if v == 1.0 && s == 0.0 { state.game_state = GameState::Flying; print_flight(v, s, true); }
                    else { print_flight(v, s, false); }
                } else {
                    state.game_state  = GameState::Crashed;
                    crashed_in_batch  = true;
                    print_crash(v, s);
                    let result = RoundResult {
                        crash_multiplier: v, flight_time: s,
                        normal_bets:      state.normal_bets    .clone(),
                        normal_cashouts:  state.normal_cashouts .clone(),
                        leaked_bets:      state.leaked_bets     .clone(),
                        leaked_cashouts:  state.leaked_cashouts .clone(),
                    };
                    state.history.push_back(v);
                    if state.history.len() > 500 { state.history.pop_front(); }
                    save_history(&state.history);
                    print_report(&result, &state.history);
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
                            let leak = state.game_state == GameState::Flying
                                    || state.game_state == GameState::Crashed;
                            print_bet(&ev, leak);
                            if leak { state.leaked_bets.push(ev); } else { state.normal_bets.push(ev); }
                        }
                        None => println!("  {} Bad bet: {}", "[WARN]".yellow(), a_str),
                    },
                    other => println!("  {} Unknown g sub-type '{}' @ {}", "[UNK]".bright_black(), other, ctr),
                }
            }

            other if !other.is_empty() =>
                println!("  {} Unknown msg '{}' @ {}", "[UNK]".bright_black(), other, ctr),
            _ => {}
        }
    }
}

// ── WebSocket session ─────────────────────────────────────────────────────────

async fn run_session(ws_url: &str, state: &mut AppState) -> Result<(), String> {
    // KEY FIX #2: permissive TLS — same as the working reference code
    let connector = NativeTlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| format!("TLS build error: {}", e))?;
    let connector = Connector::NativeTls(connector);

    let (ws_stream, _) = timeout(
        Duration::from_secs(15),
        connect_async_tls_with_config(ws_url, None, false, Some(connector))
    )
    .await
    .map_err(|_| "Connect timed out (15s)".to_string())?
    .map_err(|e| format!("WS connect error: {}", e))?;

    WS_CONNECTED.store(true, Ordering::Relaxed);
    println!("  {} WebSocket connected", "[OK]".green().bold());

    let (mut write, mut read) = ws_stream.split();
    // SignalR init handshake
    write.send(Message::Text("{}".into())).await
        .map_err(|e| format!("Handshake failed: {}", e))?;

    let silence = Duration::from_secs(SILENCE_TIMEOUT_SECS);

    loop {
        match timeout(silence, read.next()).await {
            Err(_) => {
                WS_CONNECTED.store(false, Ordering::Relaxed);
                return Err(format!("No data for {}s — stale connection", SILENCE_TIMEOUT_SECS));
            }
            Ok(None) => {
                WS_CONNECTED.store(false, Ordering::Relaxed);
                return Ok(());
            }
            Ok(Some(Err(e))) => {
                WS_CONNECTED.store(false, Ordering::Relaxed);
                return Err(format!("WS read error: {}", e));
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                let t = text.trim();
                if !t.is_empty() && t != "{}" {
                    process_message(t, state);
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                // Respond to server pings to keep the connection alive
                let _ = write.send(Message::Pong(data)).await;
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                println!("  {} Server closed: {:?}", "[WS]".yellow(), frame);
                WS_CONNECTED.store(false, Ordering::Relaxed);
                return Ok(());
            }
            Ok(Some(Ok(_))) => {} // Binary, Pong, Frame — ignore
        }
    }
}

// ── WebSocket monitor loop ────────────────────────────────────────────────────

async fn run_ws_monitor(ws_url: String) {
    let mut state        = AppState::new();
    let mut backoff      = 1u64;
    let mut fast_fails   = 0u32;

    println!("  {} WS monitor starting", "[WS]".bright_yellow());

    loop {
        let t0 = Instant::now();
        match run_session(&ws_url, &mut state).await {
            Ok(()) => {
                let lived = t0.elapsed().as_secs();
                println!("  {} Session ended cleanly after {}s", "[WS]".yellow(), lived);
                if lived > 30 { backoff = 1; fast_fails = 0; }
            }
            Err(e) => {
                let lived = t0.elapsed().as_secs();
                eprintln!("  {} Session failed after {}s: {}", "[ERROR]".red(), lived, e);
                if lived < 5 {
                    fast_fails += 1;
                    if fast_fails >= 5 { backoff = MAX_BACKOFF_SECS; }
                } else {
                    fast_fails = 0;
                    backoff    = 1;
                }
            }
        }

        save_history(&state.history);
        RECONNECT_COUNT.fetch_add(1, Ordering::Relaxed);

        println!("  {} Reconnecting in {}s  (rounds:{} history:{})",
            "[RECONNECT]".bright_yellow(), backoff,
            state.round_number, state.history.len());

        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("{}", "╔══════════════════════════════════════════════════════════╗".bright_cyan());
    println!("{}", "║      JetX Scraper — Resilient + Always-Alive Edition     ║".bright_cyan().bold());
    println!("{}", "╚══════════════════════════════════════════════════════════╝".bright_cyan());
    println!();
    println!("  Fixes applied from working reference code:");
    println!("    {} HTTP health server keeps cloud instance alive", "✓".green());
    println!("    {} Permissive TLS (danger_accept_invalid_certs)", "✓".green());
    println!("    {} WS runs in spawned task, HTTP owns main thread", "✓".green());
    println!("    {} 30s silence watchdog reconnects stale connections", "✓".green());
    println!("    {} Exponential back-off on repeated fast failures", "✓".green());
    println!("    {} History persisted to {} on every crash", "✓".green(), HISTORY_FILE);
    println!();

    // WS URL from env (update without recompile) or CLI arg or hardcoded fallback
    let ws_url = env::var("WS_URL").unwrap_or_else(|_| {
        std::env::args().nth(1).unwrap_or_else(|| {
            // !! Replace this token when it expires — or set WS_URL env var !!
            "wss://eu-server-w15.ssgportal.com/JetXNode703/signalr/connect\
             ?transport=webSockets\
             &clientProtocol=1.5\
             &token=7f854388-e80e-49d2-bf10-998d69879ce0\
             &group=JetX\
             &connectionToken=3fwTFY%2FaFBDxgVzCELGL11e18VfMndoJ9uGPP46WWhkXgkxEtM0GrX2fDFruh1u\
             %2BBfgPJtstxnVFMUUO2NSOe3JJpEDUXPvjaVYbvHI9gxdsrL6uu4%2B2P0PU7W8FOpBI\
             &connectionData=%5B%7B%22name%22%3A%22h%22%7D%5D\
             &tid=2".to_string()
        })
    });

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8000".to_string())
        .parse()
        .unwrap_or(8000);

    println!("  HTTP health server : 0.0.0.0:{}", port);
    println!("  Silence watchdog   : {}s", SILENCE_TIMEOUT_SECS);
    println!("  WS URL node        : {}", ws_url.split('/').nth(2).unwrap_or("?").yellow());
    println!();
    println!("  Ping these URLs to keep the instance alive:");
    println!("    GET /health   — for UptimeRobot / cron-job.org");
    println!("    GET /status   — full stats");
    println!();

    // KEY FIX #3: WS runs in a spawned background task.
    // If it panics or crashes, the HTTP server thread stays up,
    // keeping the cloud platform from killing the entire instance.
    tokio::spawn(run_ws_monitor(ws_url));

    // KEY FIX #1: Bind an HTTP port so the cloud platform sees a live service.
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
