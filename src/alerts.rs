// === Alert Monitor ===
//
// Отдельный процесс. Раз в 60 секунд читает БД (read-only, как watch),
// классифицирует сигналы, сравнивает с in-memory snapshot предыдущего тика
// и шлёт Telegram-сообщения о значимых tier-переходах.
//
// Anti-spam:
//   - Не более 1 сообщения per asset per 30 минут (COOLDOWN)
//   - 3+ tier-changes за 10 минут → mute этого ассета на 1 час (FLIPFLOP*)
//   - Первый тик после старта = warm-up: только startup-summary, без diff-events
//
// Логика scoring/tier ДУБЛИРУЕТСЯ из watch.rs намеренно — в задании сказано
// "не модифицируй watch", а extract в shared module это технически modify.
// Если меняешь tier-thresholds — синхронизируй ОБА места.

use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::util::format_usd;

// === Константы поведения ===
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const COOLDOWN: Duration = Duration::from_secs(30 * 60);
const FLIPFLOP_WINDOW: Duration = Duration::from_secs(10 * 60);
const FLIPFLOP_THRESHOLD: usize = 3;
const MUTE_DURATION: Duration = Duration::from_secs(60 * 60);

// === Tier (DUP from watch.rs — keep in sync) ===
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tier {
    Strong,
    Medium,
    Early,
    Weak, // в actions, но не достоин Telegram-уведомлений
}

fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::Strong => "STRONG",
        Tier::Medium => "MEDIUM",
        Tier::Early => "EARLY",
        Tier::Weak => "WEAK",
    }
}

fn tier_rank(t: Tier) -> u8 {
    match t {
        Tier::Strong => 3,
        Tier::Medium => 2,
        Tier::Early => 1,
        Tier::Weak => 0,
    }
}

fn tier_size_pct(t: Tier) -> &'static str {
    match t {
        Tier::Strong => "15-20%",
        Tier::Medium => "8-12%",
        Tier::Early => "3-5%",
        Tier::Weak => "1-2%",
    }
}

// Tier учитывается для diff-event'ов только если это S/M/E. Weak — игнорим.
fn trackable(t: Tier) -> bool {
    matches!(t, Tier::Strong | Tier::Medium | Tier::Early)
}

#[derive(Debug, Clone, Copy)]
enum TrendDir {
    Up,
    Down,
    Flat,
}

// scoring formula — DUP from watch.rs
fn compute_score(apr_now: f64, avg_apr: f64, std_pct: f64, volume: f64, dir: TrendDir) -> f64 {
    let base = (apr_now.abs() / 5.0).min(50.0);
    let ratio = (volume / 100_000.0).max(1.0);
    let volume_bonus = (ratio.log10() * 10.0).min(20.0);
    let stability_bonus = (30.0 - std_pct).max(0.0);
    let direction_bonus = if matches_trend(avg_apr, dir) { 10.0 } else { 0.0 };
    (base + volume_bonus + stability_bonus + direction_bonus).clamp(0.0, 100.0)
}

fn matches_trend(avg_apr: f64, dir: TrendDir) -> bool {
    match (avg_apr.signum(), dir) {
        (s, TrendDir::Up) if s > 0.0 => true,
        (s, TrendDir::Down) if s < 0.0 => true,
        _ => false,
    }
}

// Возвращает Some(Tier) если ассет проходит, иначе None (не шлём).
// std > 70% — отсекаем независимо от score.
fn classify_tier(score: f64, std_pct: f64, count_6h: usize) -> Option<Tier> {
    if std_pct > 70.0 {
        return None;
    }
    if score > 70.0 && std_pct < 30.0 && count_6h >= 43 {
        Some(Tier::Strong)
    } else if score > 50.0 && std_pct < 50.0 && count_6h >= 14 {
        Some(Tier::Medium)
    } else if score > 40.0 && count_6h >= 4 {
        Some(Tier::Early)
    } else if score > 30.0 {
        Some(Tier::Weak)
    } else {
        None
    }
}

// === Signal: "снимок" одного перпа на текущий тик ===
#[derive(Debug, Clone)]
struct Signal {
    asset: String,
    tier: Tier,
    score: f64,
    apr_now: f64,
    std_pct: f64,
    volume_24h: f64,
    history_h: f64, // ~ count_6h * interval / 60.0, для текста "X.Yh data"
}

// === Telegram конфиг ===
struct Telegram {
    token: String,
    chat_id: String,
}

// === Alert event diffing ===
#[derive(Debug)]
enum AlertKind {
    NewSignal, // None → Strong/Medium (Early-появление silent)
    TierUp,    // E→M, M→S
    TierDown,  // S→M, M→E
    SignalLost, // S/M/E → не в actions
}

#[derive(Debug)]
struct AlertEvent {
    asset: String,
    kind: AlertKind,
    signal: Option<Signal>,    // для LOST = None
    prev_tier: Option<Tier>,   // для NEW = None, для остальных = Some
}

// === In-memory state с rate-limiting ===
struct AlertState {
    tiers: HashMap<String, Tier>,
    last_sent: HashMap<String, Instant>,
    recent_changes: HashMap<String, Vec<Instant>>,
    muted_until: HashMap<String, Instant>,
}

impl AlertState {
    fn new() -> Self {
        Self {
            tiers: HashMap::new(),
            last_sent: HashMap::new(),
            recent_changes: HashMap::new(),
            muted_until: HashMap::new(),
        }
    }

    // Сравнить current с self.tiers, вернуть list of events.
    // Параллельно регистрирует tier-changes в recent_changes (для flip-flop детекции).
    fn diff(&mut self, current: &HashMap<String, Signal>) -> Vec<AlertEvent> {
        let now = Instant::now();
        let mut events: Vec<AlertEvent> = Vec::new();

        // Клонируем prev в локальную копию — чтобы можно было одновременно
        // мутировать self.recent_changes без borrow checker issues.
        let prev_tiers = self.tiers.clone();

        for (asset, sig) in current {
            let prev = prev_tiers.get(asset).copied();
            if prev == Some(sig.tier) {
                continue; // нет перехода
            }
            // Это tier change — регистрируем для flip-flop.
            self.record_change(asset, now);

            let evt_kind = match (prev, sig.tier) {
                // NEW: только Strong/Medium (Early появление — silent, но запоминаем)
                (None, Tier::Strong) | (None, Tier::Medium) => Some(AlertKind::NewSignal),
                (None, _) => None,
                // Transition между tracked tiers
                (Some(p), c) if trackable(p) && trackable(c) => {
                    if tier_rank(c) > tier_rank(p) {
                        Some(AlertKind::TierUp)
                    } else {
                        Some(AlertKind::TierDown)
                    }
                }
                // Weak → S/M/E или обратно — считаем как NEW/LOST соответственно?
                // Для упрощения: Weak → S/M считаем как NewSignal,
                // S/M/E → Weak считаем как SignalLost.
                (Some(Tier::Weak), Tier::Strong) | (Some(Tier::Weak), Tier::Medium) => {
                    Some(AlertKind::NewSignal)
                }
                (Some(Tier::Weak), Tier::Early) => None, // silent
                (Some(p), Tier::Weak) if trackable(p) => Some(AlertKind::SignalLost),
                _ => None,
            };

            if let Some(kind) = evt_kind {
                events.push(AlertEvent {
                    asset: asset.clone(),
                    kind,
                    signal: Some(sig.clone()),
                    prev_tier: prev,
                });
            }
        }

        // Disappeared assets (были в prev, нет в current).
        for (asset, prev_tier) in &prev_tiers {
            if !current.contains_key(asset) && trackable(*prev_tier) {
                self.record_change(asset, now);
                events.push(AlertEvent {
                    asset: asset.clone(),
                    kind: AlertKind::SignalLost,
                    signal: None,
                    prev_tier: Some(*prev_tier),
                });
            }
        }

        events
    }

    fn record_change(&mut self, asset: &str, now: Instant) {
        let log = self.recent_changes.entry(asset.to_string()).or_default();
        log.retain(|t| now.duration_since(*t) < FLIPFLOP_WINDOW);
        log.push(now);
    }

    // Проверка можно ли реально слать Telegram про этот ассет.
    // Возвращает true если можно. Применяет mute если flip-flop'ит.
    fn check_and_consume(&mut self, asset: &str) -> bool {
        let now = Instant::now();
        // 1) Mute (от прошлого flip-flop)
        if let Some(&until) = self.muted_until.get(asset) {
            if now < until {
                return false;
            }
        }
        // 2) Cooldown 30min
        if let Some(&last) = self.last_sent.get(asset) {
            if now.duration_since(last) < COOLDOWN {
                return false;
            }
        }
        // 3) Flip-flop check: если в окне 10мин уже >= 3 changes — mute и не шлём.
        let changes = self
            .recent_changes
            .get(asset)
            .map(|v| v.len())
            .unwrap_or(0);
        if changes >= FLIPFLOP_THRESHOLD {
            self.muted_until
                .insert(asset.to_string(), now + MUTE_DURATION);
            log_warn(format!(
                "Muted {} due to instability ({} changes in last {}min)",
                asset,
                changes,
                FLIPFLOP_WINDOW.as_secs() / 60
            ));
            return false;
        }
        true
    }

    fn mark_sent(&mut self, asset: &str) {
        self.last_sent.insert(asset.to_string(), Instant::now());
    }

    // После diff и отправки — записываем новое состояние.
    fn commit(&mut self, current: &HashMap<String, Signal>) {
        self.tiers.clear();
        for (asset, sig) in current {
            self.tiers.insert(asset.clone(), sig.tier);
        }
    }
}

// === Telegram send ===
async fn send_telegram(
    client: &reqwest::Client,
    tg: &Telegram,
    text: &str,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", tg.token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": tg.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        }))
        .send()
        .await
        .context("telegram POST")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Telegram API {}: {}", status, body);
    }
    Ok(())
}

async fn send_or_log(
    tg: &Option<Telegram>,
    client: &reqwest::Client,
    text: &str,
) {
    match tg {
        Some(t) => {
            if let Err(e) = send_telegram(client, t, text).await {
                log_warn(format!("telegram send failed: {:#}", e));
            }
        }
        None => {
            // Без креденшелов — печатаем что бы отправили (для observability).
            log_info(format!("[telegram-disabled] would send:\n{}", text));
        }
    }
}

// === Message rendering ===
fn render_event(event: &AlertEvent) -> String {
    let header = match (&event.kind, event.prev_tier, &event.signal) {
        (AlertKind::NewSignal, _, Some(sig)) => {
            format!("🆕 NEW {} SIGNAL", tier_label(sig.tier))
        }
        (AlertKind::TierUp, Some(prev), Some(sig)) => {
            format!("📈 TIER UP: {} → {}", tier_label(prev), tier_label(sig.tier))
        }
        (AlertKind::TierDown, Some(prev), Some(sig)) => {
            format!(
                "📉 TIER DOWN: {} → {}",
                tier_label(prev),
                tier_label(sig.tier)
            )
        }
        (AlertKind::SignalLost, Some(prev), _) => {
            format!("❌ SIGNAL LOST (was {})", tier_label(prev))
        }
        _ => "ALERT".to_string(),
    };

    if let Some(sig) = &event.signal {
        let side = if sig.apr_now < 0.0 { "LONG" } else { "SHORT" };
        let daily_yield = sig.apr_now.abs() / 365.0;
        let action = format!("{} to earn ${:.2}/day per $100", side, daily_yield);
        let confidence_detail = format!(
            "{} ({:.1}h data, std {:.0}%)",
            tier_label(sig.tier),
            sig.history_h,
            sig.std_pct
        );
        format!(
            "{}\nAsset: {}\nAction: {}\nScore: {:.0}/100\nConfidence: {}\nVolume 24h: {}\nRecommended size: {} of capital\n\n⚠️ Educational signal only. Verify before trading.",
            header,
            sig.asset,
            action,
            sig.score,
            confidence_detail,
            format_usd(sig.volume_24h),
            tier_size_pct(sig.tier),
        )
    } else {
        format!(
            "{}\nAsset: {}\nSignal dropped from action list.",
            header, event.asset,
        )
    }
}

// === Compute signals from DB (DUP-ish от watch::fetch_state, но только actions) ===
async fn compute_signals(pool: &SqlitePool) -> Result<HashMap<String, Signal>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Детектим cadence (для расчёта "history_h" из count_6h).
    let cadence_sec: i64 = {
        let r: Vec<(i64,)> = sqlx::query_as(
            "SELECT timestamp FROM \
             (SELECT DISTINCT timestamp FROM funding_snapshots ORDER BY timestamp DESC LIMIT 2)",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if r.len() == 2 {
            (r[0].0 - r[1].0).abs().max(1)
        } else {
            300
        }
    };

    // 1h данные — для apr_now / std_pct / direction
    let cutoff_1h = now - 3600;
    let raw_1h: Vec<(String, f64, f64, i64)> = sqlx::query_as(
        "SELECT asset, funding_apr, day_volume_usd, timestamp \
         FROM funding_snapshots \
         WHERE timestamp >= ? \
         ORDER BY asset, timestamp DESC",
    )
    .bind(cutoff_1h)
    .fetch_all(pool)
    .await
    .context("alerts 1h query")?;

    let mut by_asset_1h: HashMap<String, Vec<(f64, f64, i64)>> = HashMap::new();
    for (asset, apr, vol, ts) in raw_1h {
        by_asset_1h.entry(asset).or_default().push((apr, vol, ts));
    }

    // 6h — только counts per asset (для tier qualifier и history_h).
    let cutoff_6h = now - 6 * 3600;
    let raw_6h: Vec<(String,)> = sqlx::query_as(
        "SELECT asset FROM funding_snapshots WHERE timestamp >= ?",
    )
    .bind(cutoff_6h)
    .fetch_all(pool)
    .await
    .context("alerts 6h count query")?;

    let mut count_6h: HashMap<String, usize> = HashMap::new();
    for (asset,) in raw_6h {
        *count_6h.entry(asset).or_insert(0) += 1;
    }

    let min_count_1h: usize = ((12.0_f64) * 0.6).ceil() as usize; // = 8

    let mut signals: HashMap<String, Signal> = HashMap::new();
    for (asset, snaps) in &by_asset_1h {
        if snaps.len() < min_count_1h {
            continue;
        }
        let n = snaps.len() as f64;
        let last = snaps[0];
        let apr_now = last.0;
        let volume = last.1;

        let sum: f64 = snaps.iter().map(|s| s.0).sum();
        let sum_sq: f64 = snaps.iter().map(|s| s.0 * s.0).sum();
        let avg = sum / n;
        let var = (sum_sq / n - avg * avg).max(0.0);
        let std = var.sqrt();
        let std_pct = if avg.abs() > 1e-9 {
            100.0 * std / avg.abs()
        } else {
            0.0
        };

        let direction = if snaps.len() >= 3 {
            let v_old = snaps[2].0;
            let v_mid = snaps[1].0;
            let v_new = snaps[0].0;
            if v_new > v_mid && v_mid > v_old {
                TrendDir::Up
            } else if v_new < v_mid && v_mid < v_old {
                TrendDir::Down
            } else {
                TrendDir::Flat
            }
        } else {
            TrendDir::Flat
        };

        // TRAP: алерты не шлём (в задании не указано).
        let is_trap = apr_now.abs() > 500.0 && volume < 1_000_000.0;
        if is_trap {
            continue;
        }

        let score = compute_score(apr_now, avg, std_pct, volume, direction);
        let cnt6 = count_6h.get(asset).copied().unwrap_or(0);
        let tier = match classify_tier(score, std_pct, cnt6) {
            Some(t) => t,
            None => continue,
        };

        let history_h = (cnt6 as f64) * (cadence_sec as f64) / 3600.0;
        signals.insert(
            asset.clone(),
            Signal {
                asset: asset.clone(),
                tier,
                score,
                apr_now,
                std_pct,
                volume_24h: volume,
                history_h,
            },
        );
    }

    Ok(signals)
}

// === DB pool — read-only ===
async fn open_readonly_pool() -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite://./data/snapshots.db")
        .context("parse sqlite URL")?
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .context(
            "не удалось открыть ./data/snapshots.db в read-only режиме \
             (нет файла? сначала запусти `cargo run -- collect`)",
        )?;
    Ok(pool)
}

// === Logging helpers (формат как у collect) ===
fn log_info(msg: impl AsRef<str>) {
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    println!("[{}] {}", ts, msg.as_ref());
}

fn log_warn(msg: impl AsRef<str>) {
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    eprintln!("[{}] WARN: {}", ts, msg.as_ref());
}

// === Точка входа ===
pub async fn run() -> Result<()> {
    // Читаем env vars один раз при старте. Пустые значения тоже считаем "не задано".
    let token = env::var("TELEGRAM_BOT_TOKEN").ok().filter(|s| !s.is_empty());
    let chat = env::var("TELEGRAM_CHAT_ID").ok().filter(|s| !s.is_empty());

    let telegram = match (token, chat) {
        (Some(t), Some(c)) => {
            log_info(format!("Telegram configured for chat_id={}", c));
            Some(Telegram { token: t, chat_id: c })
        }
        _ => {
            log_warn(
                "TELEGRAM_BOT_TOKEN or TELEGRAM_CHAT_ID not set — \
                 alerts disabled (will log would-send messages to stdout)",
            );
            None
        }
    };

    let pool = open_readonly_pool().await?;
    let client = reqwest::Client::new();

    log_info(format!(
        "Starting alert-monitor, poll: {}s, db: ./data/snapshots.db",
        POLL_INTERVAL.as_secs()
    ));

    let mut state = AlertState::new();
    let mut first_iteration = true;

    loop {
        let signals = match compute_signals(&pool).await {
            Ok(s) => s,
            Err(e) => {
                log_warn(format!("compute_signals error: {:#}", e));
                // Спим и пробуем снова.
                if interruptible_sleep(POLL_INTERVAL).await {
                    pool.close().await;
                    return Ok(());
                }
                continue;
            }
        };

        let (n_strong, n_medium, n_early, n_weak) = count_tiers(&signals);

        if first_iteration {
            // Warm-up: state.tiers пустой. Diff бы выкинул NEW для всех signals
            // — это спам. Просто фиксируем state и шлём startup-summary.
            state.commit(&signals);
            let summary = format!(
                "🟢 Alert monitor started, tracking {} assets, current tiers: STRONG={}, MEDIUM={}, EARLY={}",
                signals.len(),
                n_strong,
                n_medium,
                n_early,
            );
            log_info(&summary);
            send_or_log(&telegram, &client, &summary).await;
            first_iteration = false;
        } else {
            log_info(format!(
                "tick: {} signals (S={}, M={}, E={}, W={})",
                signals.len(),
                n_strong,
                n_medium,
                n_early,
                n_weak
            ));
            let events = state.diff(&signals);
            for event in events {
                if !state.check_and_consume(&event.asset) {
                    continue;
                }
                let msg = render_event(&event);
                log_info(format!(
                    "alert {}: {} (prev={:?}, new={:?})",
                    event_kind_str(&event.kind),
                    event.asset,
                    event.prev_tier.map(tier_label),
                    event.signal.as_ref().map(|s| tier_label(s.tier)),
                ));
                send_or_log(&telegram, &client, &msg).await;
                state.mark_sent(&event.asset);
            }
            state.commit(&signals);
        }

        if interruptible_sleep(POLL_INTERVAL).await {
            log_info("Shutdown signal received, exiting cleanly");
            pool.close().await;
            return Ok(());
        }
    }
}

fn count_tiers(signals: &HashMap<String, Signal>) -> (usize, usize, usize, usize) {
    let mut s = 0;
    let mut m = 0;
    let mut e = 0;
    let mut w = 0;
    for sig in signals.values() {
        match sig.tier {
            Tier::Strong => s += 1,
            Tier::Medium => m += 1,
            Tier::Early => e += 1,
            Tier::Weak => w += 1,
        }
    }
    (s, m, e, w)
}

fn event_kind_str(k: &AlertKind) -> &'static str {
    match k {
        AlertKind::NewSignal => "NEW",
        AlertKind::TierUp => "UP",
        AlertKind::TierDown => "DOWN",
        AlertKind::SignalLost => "LOST",
    }
}

// Спит до timeout или Ctrl+C. Возвращает true если был сигнал → caller должен выйти.
async fn interruptible_sleep(dur: Duration) -> bool {
    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}
