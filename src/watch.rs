// === Watch: live TUI dashboard ===
//
// Read-only TUI поверх ./data/snapshots.db. Полностью изолирован от collect mode:
// открываем sqlite в read-only режиме, ставим busy_timeout чтобы не падать
// если коллектор в этот момент пишет (PRAGMA busy_timeout заставляет sqlite
// ретраить внутренне).
//
// Цикл: каждые 5с (или по нажатию 'r') пересчитываем State из БД, отрисовываем.
// 'q' / Ctrl+C — выход с восстановлением терминала.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction as LayoutDir, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::util::{format_bytes, format_usd};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

// === Точка входа модуля ===
pub async fn run() -> Result<()> {
    // Read-only пул. Не создаст файл если его нет — упадёт с осмысленной ошибкой.
    let pool = open_readonly_pool().await?;

    // Если код запаникует посреди raw mode — обычная trace ляжет в покорёженный
    // терминал. Хук это лечит: восстановит state до того как trace напечатается.
    install_panic_hook();

    let mut terminal = setup_terminal().context("setup terminal")?;

    // Главный цикл; что бы он ни вернул — обязательно восстановим терминал.
    let result = run_loop(&mut terminal, &pool).await;
    let _ = restore_terminal(&mut terminal);
    pool.close().await;

    result
}

// === SQLite read-only пул ===
//
// .read_only(true) → SQLITE_OPEN_READONLY (не создаёт файл, не даёт писать).
// .busy_timeout(5s) → PRAGMA busy_timeout=5000ms; sqlite будет ретраить
//   внутри C-библиотеки если другая коннекция держит lock.
// max_connections=2 — для read-only хватит, не нагружаем БД.
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

// === Terminal setup / teardown ===

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

// === State ===
//
// Снимок данных, отдаваемый в draw_ui. Считается один раз за цикл рефреша.
// Все async-операции SQL — здесь, draw — чисто синхронная функция.

#[derive(Debug, Clone)]
struct State {
    fetch_time: SystemTime,
    db_size: u64,
    total_snapshots: i64,
    last_snapshot_ts: Option<i64>,
    first_snapshot_ts: Option<i64>,
    perps_in_last: i64,
    interval_seconds: Option<i64>,
    movers: Vec<MoverRow>,
    actions: Vec<ActionRow>,
    candidates: Vec<CandidateRow>,
    section2_msg: Option<String>,
    action_msg: Option<String>,
    section3_msg: Option<String>,
    fetch_error: Option<String>,
}

#[derive(Debug, Clone)]
struct MoverRow {
    asset: String,
    apr_now: f64,
    avg_1h: f64,
    std_1h: f64,
    direction: TrendDir,
    volume_24h: f64,
}

#[derive(Debug, Clone, Copy)]
enum TrendDir {
    Up,
    Down,
    Flat,
}

#[derive(Debug, Clone)]
struct CandidateRow {
    asset: String,
    avg_apr: f64,
    std_apr: f64,
    snap_count: i64,
    score: f64,
}

// === Action Signals ===
//
// Tier по доступности данных + качеству. TRAP — отдельная категория-предупреждение,
// показываем но рекомендации не даём.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConfidenceTier {
    Strong, // >= 6h данных, std<30%, score>70
    Medium, // >= 2h данных, std<50%, score>50
    Early,  // >= 30min данных, score>40
    Weak,   // score>30
    Trap,   // |APR|>500% + vol<$1M
}

#[derive(Debug, Clone)]
struct ActionRow {
    asset: String,
    action: String,        // "LONG to earn $0.27/day per $100" или "TRAP — possible manipulation"
    score: f64,            // 0..100, для TRAP = 0 (не показываем)
    tier: ConfidenceTier,
    size_range: &'static str, // "$150-200", "$80-120", "—" для трапов
    risks: String,
}

// === Score formula ===
//
// 0..100 баллов, выше = лучше сетап. Слагаемые суммируются и клампятся.
//   base       0..50   APR contribution; 50 баллов при |APR| >= 250%
//   volume     0..20   ликвидность (log10), 20 баллов при vol >= $10M
//   stability  0..30   30 - std_pct, отрицательное обнуляется
//   direction  0/10    +10 если тренд совпадает со знаком среднего
fn compute_score(apr_now: f64, avg_apr: f64, std_pct: f64, volume: f64, dir: TrendDir) -> f64 {
    let base = (apr_now.abs() / 5.0).min(50.0);
    // log10 безопасно: collect-фильтр гарантирует vol > $100k → ratio >= 1 → log10 >= 0.
    // На всякий случай — clamp ratio к минимуму 1.0 (защита от старых записей).
    let ratio = (volume / 100_000.0).max(1.0);
    let volume_bonus = (ratio.log10() * 10.0).min(20.0);
    let stability_bonus = (30.0 - std_pct).max(0.0);
    let direction_bonus = if matches_trend(avg_apr, dir) { 10.0 } else { 0.0 };
    (base + volume_bonus + stability_bonus + direction_bonus).clamp(0.0, 100.0)
}

// Тренд "матчит" знак среднего если: avg>0 и направление вверх, или avg<0 и вниз.
// Flat и противоположный — не матчит.
fn matches_trend(avg_apr: f64, dir: TrendDir) -> bool {
    match (avg_apr.signum(), dir) {
        (s, TrendDir::Up) if s > 0.0 => true,
        (s, TrendDir::Down) if s < 0.0 => true,
        _ => false,
    }
}

// Tier классификация. Возвращает None если не проходит даже WEAK или std слишком высокий.
// count_6h — сколько снапшотов у этого ассета в 6h-окне (proxy для "сколько данных").
fn classify_tier(score: f64, std_pct: f64, count_6h: usize) -> Option<ConfidenceTier> {
    // std > 70% — отсекаем независимо от score: слишком волатильно для сделки.
    if std_pct > 70.0 {
        return None;
    }
    // Минимальные snap_count для каждого тира при 5-min cadence × 60% threshold:
    //   STRONG: 6h * 12 * 0.6 ≈ 43
    //   MEDIUM: 2h * 12 * 0.6 ≈ 14
    //   EARLY:  30min/5min * 0.6 ≈ 4
    if score > 70.0 && std_pct < 30.0 && count_6h >= 43 {
        Some(ConfidenceTier::Strong)
    } else if score > 50.0 && std_pct < 50.0 && count_6h >= 14 {
        Some(ConfidenceTier::Medium)
    } else if score > 40.0 && count_6h >= 4 {
        Some(ConfidenceTier::Early)
    } else if score > 30.0 {
        Some(ConfidenceTier::Weak)
    } else {
        None
    }
}

fn tier_size(tier: ConfidenceTier) -> &'static str {
    match tier {
        ConfidenceTier::Strong => "$150-200",
        ConfidenceTier::Medium => "$80-120",
        ConfidenceTier::Early => "$30-50",
        ConfidenceTier::Weak => "$10-20",
        ConfidenceTier::Trap => "—",
    }
}

fn tier_label(tier: ConfidenceTier) -> &'static str {
    match tier {
        ConfidenceTier::Strong => "STRONG",
        ConfidenceTier::Medium => "MEDIUM",
        ConfidenceTier::Early => "EARLY",
        ConfidenceTier::Weak => "WEAK",
        ConfidenceTier::Trap => "TRAP",
    }
}

fn tier_color(tier: ConfidenceTier) -> Color {
    match tier {
        ConfidenceTier::Strong => Color::Green,
        ConfidenceTier::Medium => Color::Cyan,
        ConfidenceTier::Early => Color::Yellow,
        ConfidenceTier::Weak => Color::Gray,
        ConfidenceTier::Trap => Color::Red,
    }
}

fn score_color(score: f64) -> Color {
    if score >= 70.0 {
        Color::Green
    } else if score >= 50.0 {
        Color::Yellow
    } else {
        // оранжевый — xterm-256 индекс 208, ratatui рендерит на терминалах с 256+ цветов
        Color::Indexed(208)
    }
}

fn build_risks(volume: f64, std_pct: f64, count_6h: usize) -> String {
    let mut risks: Vec<&str> = Vec::new();
    if volume < 500_000.0 {
        risks.push("Low volume");
    }
    if std_pct > 50.0 {
        risks.push("High volatility");
    }
    if count_6h < 12 {
        // < 1h эквивалент данных
        risks.push("New signal");
    }
    if risks.is_empty() {
        "Liquid + stable".to_string()
    } else {
        risks.join(", ")
    }
}

impl State {
    // Конструктор для случая "fetch упал" — рисуем минимальный State с error.
    fn from_error(e: anyhow::Error) -> Self {
        State {
            fetch_time: SystemTime::now(),
            db_size: 0,
            total_snapshots: 0,
            last_snapshot_ts: None,
            first_snapshot_ts: None,
            perps_in_last: 0,
            interval_seconds: None,
            movers: vec![],
            actions: vec![],
            candidates: vec![],
            section2_msg: Some("(no data — DB error)".to_string()),
            action_msg: Some("(no data — DB error)".to_string()),
            section3_msg: Some("(no data — DB error)".to_string()),
            fetch_error: Some(format!("{:#}", e)),
        }
    }
}

// === Главный цикл ===
async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    pool: &SqlitePool,
) -> Result<()> {
    let mut state = match fetch_state(pool).await {
        Ok(s) => s,
        Err(e) => State::from_error(e),
    };
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|f| draw_ui(f, &state))?;

        // event::poll блокирует до timeout или до прихода события.
        // 100ms — компромисс между отзывчивостью на нажатия и нагрузкой.
        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) => break,
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('r'), _) => {
                        state = match fetch_state(pool).await {
                            Ok(s) => s,
                            Err(e) => State::from_error(e),
                        };
                        last_refresh = Instant::now();
                    }
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            state = match fetch_state(pool).await {
                Ok(s) => s,
                Err(e) => State::from_error(e),
            };
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

// === Fetch state from DB ===
async fn fetch_state(pool: &SqlitePool) -> Result<State> {
    // -- Header counters --
    let head: (i64, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(timestamp), MIN(timestamp) FROM funding_snapshots",
    )
    .fetch_one(pool)
    .await
    .context("header counters")?;

    let total_snapshots = head.0;
    let last_snapshot_ts = head.1;
    let first_snapshot_ts = head.2;

    // Сколько перпов в последнем снапшоте (= rows с timestamp = MAX(ts)).
    let perps_in_last: i64 = if let Some(ts) = last_snapshot_ts {
        sqlx::query_scalar("SELECT COUNT(*) FROM funding_snapshots WHERE timestamp = ?")
            .bind(ts)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    } else {
        0
    };

    // Интервал между последними двумя снапшотами (детектим cadence).
    let interval_seconds: Option<i64> = {
        let r: Vec<(i64,)> = sqlx::query_as(
            "SELECT timestamp FROM \
             (SELECT DISTINCT timestamp FROM funding_snapshots ORDER BY timestamp DESC LIMIT 2)",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if r.len() == 2 {
            Some((r[0].0 - r[1].0).abs())
        } else {
            None
        }
    };

    let db_size = std::fs::metadata("./data/snapshots.db")
        .map(|m| m.len())
        .unwrap_or(0);

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;

    // -- Section 2: 1h top movers --
    //
    // Тащим все строки за последний час одной выборкой (1750 rows max),
    // группируем по ассету в Rust. Дешевле чем CTE с window funcs для нашего кейса.
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
    .context("section2 query")?;

    let mut by_asset_1h: HashMap<String, Vec<(f64, f64, i64)>> = HashMap::new();
    for (asset, apr, vol, ts) in raw_1h {
        by_asset_1h.entry(asset).or_default().push((apr, vol, ts));
    }

    // 60% от ожидаемых 12 снапшотов в час = ceil(7.2) = 8.
    let min_count_1h: usize = ((12.0 * 0.6) as f64).ceil() as usize;

    // Промежуточная структура: считаем per-asset stats один раз,
    // потом переиспользуем для movers И actions.
    struct Stats1h {
        asset: String,
        apr_now: f64,
        avg: f64,
        std: f64,
        std_pct: f64,
        direction: TrendDir,
        volume_24h: f64,
    }

    let mut all_stats: Vec<Stats1h> = Vec::new();
    for (asset, snaps) in &by_asset_1h {
        if snaps.len() < min_count_1h {
            continue;
        }
        let n = snaps.len() as f64;
        let last = snaps[0]; // DESC, самый свежий первый
        let apr_now = last.0;
        let volume_24h = last.1;

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

        // Trend по последним 3 (в DESC, переворачиваем: oldest..newest).
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

        all_stats.push(Stats1h {
            asset: asset.clone(),
            apr_now,
            avg,
            std,
            std_pct,
            direction,
            volume_24h,
        });
    }

    let mut movers: Vec<MoverRow> = all_stats
        .iter()
        .map(|s| MoverRow {
            asset: s.asset.clone(),
            apr_now: s.apr_now,
            avg_1h: s.avg,
            std_1h: s.std,
            direction: s.direction,
            volume_24h: s.volume_24h,
        })
        .collect();
    movers.sort_by(|a, b| {
        b.apr_now
            .abs()
            .partial_cmp(&a.apr_now.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    movers.truncate(15);

    let section2_msg = if movers.is_empty() {
        let history_min = match (first_snapshot_ts, last_snapshot_ts) {
            (Some(f), Some(l)) => (l - f) / 60,
            _ => 0,
        };
        Some(format!(
            "Need more data: have ~{} min of history, need at least 1h with ≥{} snapshots per asset",
            history_min, min_count_1h
        ))
    } else {
        None
    };

    // -- Pull 6h data (нужно и для actions count_6h, и для candidates) --
    let cutoff_6h = now - 6 * 3600;
    let raw_6h: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT asset, funding_apr, day_volume_usd \
         FROM funding_snapshots \
         WHERE timestamp >= ? \
         ORDER BY asset, timestamp DESC",
    )
    .bind(cutoff_6h)
    .fetch_all(pool)
    .await
    .context("section3 query")?;

    let mut by_asset_6h: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for (asset, apr, vol) in raw_6h {
        by_asset_6h.entry(asset).or_default().push((apr, vol));
    }

    // 60% от 6h*12 = 43.
    let min_count_6h: usize = (6.0 * 12.0 * 0.6f64).ceil() as usize;

    let history_seconds = match (first_snapshot_ts, last_snapshot_ts) {
        (Some(f), Some(l)) => l - f,
        _ => 0,
    };

    // -- Section 3 (NEW position-wise): Action Signals --
    //
    // Используем уже посчитанные all_stats (1h окно) + count_6h из by_asset_6h.
    // Логика: сначала trap-чек (показываем как warning), иначе компьютим score
    // и tier; если ни tier нет — отбрасываем. Сортировка: traps первыми (важно
    // увидеть), потом по score убывающе. Топ-10.
    let mut actions: Vec<ActionRow> = Vec::new();
    for s in &all_stats {
        let count_6h = by_asset_6h.get(&s.asset).map(|v| v.len()).unwrap_or(0);

        // TRAP: |APR| > 500% И ликвидность < $1M → варнинг, без рекомендации.
        let is_trap = s.apr_now.abs() > 500.0 && s.volume_24h < 1_000_000.0;
        if is_trap {
            actions.push(ActionRow {
                asset: s.asset.clone(),
                action: "TRAP — possible manipulation".to_string(),
                score: 0.0,
                tier: ConfidenceTier::Trap,
                size_range: tier_size(ConfidenceTier::Trap),
                risks: format!(
                    "APR {:+.0}% with vol {} < $1M",
                    s.apr_now,
                    format_usd(s.volume_24h)
                ),
            });
            continue;
        }

        let score = compute_score(s.apr_now, s.avg, s.std_pct, s.volume_24h, s.direction);
        let tier = match classify_tier(score, s.std_pct, count_6h) {
            Some(t) => t,
            None => continue, // не дотягивает до WEAK или std > 70%
        };

        // daily yield в долларах при позиции на $100.
        // Из APR в %/год: per-day = apr_pct / 365 / 100 * 100 = apr_pct / 365 (в $).
        let daily_yield = s.apr_now.abs() / 365.0;
        let side = if s.apr_now < 0.0 { "LONG" } else { "SHORT" };
        let action = format!("{} to earn ${:.2}/day per $100", side, daily_yield);
        let risks = build_risks(s.volume_24h, s.std_pct, count_6h);

        actions.push(ActionRow {
            asset: s.asset.clone(),
            action,
            score,
            tier,
            size_range: tier_size(tier),
            risks,
        });
    }
    actions.sort_by(|a, b| {
        // TRAP первыми (warning важнее score'а), потом по score desc.
        let a_trap = if a.tier == ConfidenceTier::Trap { 0 } else { 1 };
        let b_trap = if b.tier == ConfidenceTier::Trap { 0 } else { 1 };
        a_trap.cmp(&b_trap).then_with(|| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    actions.truncate(10);

    let action_msg = if actions.is_empty() {
        if all_stats.is_empty() {
            let history_min = match (first_snapshot_ts, last_snapshot_ts) {
                (Some(f), Some(l)) => (l - f) / 60,
                _ => 0,
            };
            Some(format!(
                "Need at least 1h of data; currently have ~{} min",
                history_min
            ))
        } else {
            Some("No action signals match scoring filters yet.".to_string())
        }
    } else {
        None
    };

    // -- Section 4: 6h persistent candidates --
    let mut candidates: Vec<CandidateRow> = Vec::new();
    for (asset, snaps) in &by_asset_6h {
        let cnt = snaps.len();
        if cnt < min_count_6h {
            continue;
        }
        let n = cnt as f64;
        let sum: f64 = snaps.iter().map(|s| s.0).sum();
        let sum_sq: f64 = snaps.iter().map(|s| s.0 * s.0).sum();
        let avg = sum / n;
        let var = (sum_sq / n - avg * avg).max(0.0);
        let std = var.sqrt();
        let last_volume = snaps[0].1;

        if avg.abs() < 30.0 {
            continue;
        }
        if std >= avg.abs() * 0.5 {
            continue;
        }
        if last_volume < 1_000_000.0 {
            continue;
        }

        // score = |avg| * (1 - std/|avg|). При std=0 → score = |avg|.
        // При std=|avg| → score = 0. Persistence-фильтр гарантирует score > 0.5*|avg|.
        let score = avg.abs() * (1.0 - std / avg.abs());

        candidates.push(CandidateRow {
            asset: asset.clone(),
            avg_apr: avg,
            std_apr: std,
            snap_count: cnt as i64,
            score,
        });
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let section3_msg = if candidates.is_empty() {
        if history_seconds < 6 * 3600 {
            Some(format!(
                "No candidates yet. Persistent analysis needs ≥6h of data, currently have {:.1}h",
                history_seconds as f64 / 3600.0
            ))
        } else {
            Some(
                "No persistent candidates matching filters yet. Market may be in transition."
                    .to_string(),
            )
        }
    } else {
        None
    };

    Ok(State {
        fetch_time: SystemTime::now(),
        db_size,
        total_snapshots,
        last_snapshot_ts,
        first_snapshot_ts,
        perps_in_last,
        interval_seconds,
        movers,
        actions,
        candidates,
        section2_msg,
        action_msg,
        section3_msg,
        fetch_error: None,
    })
}

// === Rendering ===

fn draw_ui(f: &mut Frame, state: &State) {
    let chunks = Layout::default()
        .direction(LayoutDir::Vertical)
        .constraints([
            Constraint::Length(8),  // header
            Constraint::Min(8),     // movers
            Constraint::Min(8),     // action signals
            Constraint::Min(8),     // persistent candidates
            Constraint::Length(2),  // footer (disclaimer + controls)
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_movers(f, chunks[1], state);
    draw_actions(f, chunks[2], state);
    draw_candidates(f, chunks[3], state);
    draw_footer(f, chunks[4], state);
}

fn draw_header(f: &mut Frame, area: Rect, state: &State) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;

    // Status: COLLECTING / STALE / NO DATA
    let status_line: Line = if let Some(ts) = state.last_snapshot_ts {
        let age = now - ts;
        let interval = state.interval_seconds.unwrap_or(300);
        if age <= interval * 2 {
            Line::from(vec![
                Span::raw("Status: "),
                Span::styled(
                    "● COLLECTING",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            Line::from(vec![
                Span::raw("Status: "),
                Span::styled(
                    "● STALE",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " (last snapshot {}s ago, expected every {}s)",
                    age, interval
                )),
            ])
        }
    } else {
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(
                "● NO DATA",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ])
    };

    let last_snap_line = if let Some(ts) = state.last_snapshot_ts {
        let age = now - ts;
        let dt_str = Utc
            .timestamp_opt(ts, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "?".to_string());
        format!("Last snapshot: {} ({}s ago)", dt_str, age)
    } else {
        "Last snapshot: none".to_string()
    };

    let rate_line = format!(
        "Snapshot rate: {} perps / {}",
        state.perps_in_last,
        state
            .interval_seconds
            .map(|s| if s % 60 == 0 {
                format!("{}min", s / 60)
            } else {
                format!("{}s", s)
            })
            .unwrap_or_else(|| "?".to_string()),
    );

    let uptime_line =
        if let (Some(f0), Some(l)) = (state.first_snapshot_ts, state.last_snapshot_ts) {
            let span_h = (l - f0) as f64 / 3600.0;
            format!("Uptime tracking: {:.1}h of data", span_h)
        } else {
            "Uptime tracking: -".to_string()
        };

    let lines = vec![
        status_line,
        Line::from(format!(
            "DB: ./data/snapshots.db ({})",
            format_bytes(state.db_size)
        )),
        Line::from(format!("Total snapshots: {}", state.total_snapshots)),
        Line::from(last_snap_line),
        Line::from(rate_line),
        Line::from(uptime_line),
    ];

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Hyperliquid Funding Scanner "),
    );
    f.render_widget(para, area);
}

fn apr_color(apr: f64) -> Color {
    let abs = apr.abs();
    if abs > 100.0 {
        Color::Red
    } else if abs > 50.0 {
        Color::Yellow
    } else {
        Color::White
    }
}

fn std_color(avg: f64, std: f64) -> Color {
    if avg.abs() < 1e-9 {
        Color::White
    } else {
        let ratio = std / avg.abs();
        if ratio < 0.3 {
            Color::Green
        } else if ratio > 0.7 {
            Color::Red
        } else {
            Color::White
        }
    }
}

fn dir_arrow(d: TrendDir) -> &'static str {
    match d {
        TrendDir::Up => "↑",
        TrendDir::Down => "↓",
        TrendDir::Flat => "→",
    }
}

fn side_str(apr: f64) -> &'static str {
    // Положительный funding → лонги платят шортам → SHORT для приёма.
    // Отрицательный funding → шорты платят лонгам → LONG.
    if apr >= 0.0 { "SHORT" } else { "LONG" }
}

fn side_color(apr: f64) -> Color {
    if apr >= 0.0 { Color::Red } else { Color::Green }
}

fn draw_movers(f: &mut Frame, area: Rect, state: &State) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Top Movers (1h window, by |APR Now|) ");

    if let Some(msg) = &state.section2_msg {
        let para = Paragraph::new(msg.as_str()).block(block);
        f.render_widget(para, area);
        return;
    }

    let header = Row::new(vec![
        "Asset", "APR Now", "1h Avg", "1h Std%", "Trend", "Vol 24h", "Side",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .movers
        .iter()
        .map(|m| {
            let std_pct = if m.avg_1h.abs() > 1e-9 {
                100.0 * m.std_1h / m.avg_1h.abs()
            } else {
                0.0
            };
            Row::new(vec![
                Cell::from(m.asset.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("{:+.2}%", m.apr_now))
                    .style(Style::default().fg(apr_color(m.apr_now))),
                Cell::from(format!("{:+.2}%", m.avg_1h)),
                Cell::from(format!("{:.0}%", std_pct))
                    .style(Style::default().fg(std_color(m.avg_1h, m.std_1h))),
                Cell::from(dir_arrow(m.direction)),
                Cell::from(format_usd(m.volume_24h)),
                Cell::from(side_str(m.apr_now)).style(Style::default().fg(side_color(m.apr_now))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_actions(f: &mut Frame, area: Rect, state: &State) {
    let block = Block::default().borders(Borders::ALL).title(
        " Action Signals (Educational signals only. Earlier signals = higher risk.) ",
    );

    if let Some(msg) = &state.action_msg {
        let para = Paragraph::new(msg.as_str()).block(block);
        f.render_widget(para, area);
        return;
    }

    let header = Row::new(vec![
        "Asset",
        "Action",
        "Score",
        "Confidence",
        "Size",
        "Risks",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .actions
        .iter()
        .map(|a| {
            let score_cell = if a.tier == ConfidenceTier::Trap {
                Cell::from("—")
            } else {
                Cell::from(format!("{:.0}", a.score))
                    .style(Style::default().fg(score_color(a.score)))
            };
            // Side в Action подсвечиваем по знаку: для TRAP — красный весь action
            let action_style = if a.tier == ConfidenceTier::Trap {
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(a.asset.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(a.action.clone()).style(action_style),
                score_cell,
                Cell::from(tier_label(a.tier)).style(
                    Style::default()
                        .fg(tier_color(a.tier))
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(a.size_range),
                Cell::from(a.risks.clone()),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(38),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_candidates(f: &mut Frame, area: Rect, state: &State) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Persistent Candidates (6h, |APR|≥30%, std/|avg|<0.5, vol≥$1M) ");

    if let Some(msg) = &state.section3_msg {
        let para = Paragraph::new(msg.as_str()).block(block);
        f.render_widget(para, area);
        return;
    }

    let header = Row::new(vec![
        "Asset", "Avg APR 6h", "Std%", "Snapshots", "Side", "Score",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .candidates
        .iter()
        .map(|c| {
            let std_pct = if c.avg_apr.abs() > 1e-9 {
                100.0 * c.std_apr / c.avg_apr.abs()
            } else {
                0.0
            };
            Row::new(vec![
                Cell::from(c.asset.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("{:+.2}%", c.avg_apr))
                    .style(Style::default().fg(apr_color(c.avg_apr))),
                Cell::from(format!("{:.0}%", std_pct))
                    .style(Style::default().fg(std_color(c.avg_apr, c.std_apr))),
                Cell::from(c.snap_count.to_string()),
                Cell::from(side_str(c.avg_apr))
                    .style(Style::default().fg(side_color(c.avg_apr))),
                Cell::from(format!("{:.1}", c.score))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_footer(f: &mut Frame, area: Rect, state: &State) {
    let secs = state
        .fetch_time
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let last_update = Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|d| d.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string());

    // Строка 1: дисклеймер про signals (важнее звучит чем красный — желтый чтобы заметили).
    let disclaimer = Line::from(Span::styled(
        "Signals ≠ profits. Always paper trade first. Hyperliquid funding ≠ delta-neutral without hedge.",
        Style::default().fg(Color::Yellow),
    ));

    // Строка 2: контролы + опционально DB-ошибка.
    let controls_text = if let Some(err) = &state.fetch_error {
        format!(
            "⚠ DB error: {} | 'q' quit | 'r' refresh | Last update: {}",
            err, last_update
        )
    } else {
        format!(
            "'q' quit | 'r' refresh | Last update: {}",
            last_update
        )
    };
    let controls = Line::from(Span::styled(
        controls_text,
        Style::default().fg(Color::DarkGray),
    ));

    let para = Paragraph::new(vec![disclaimer, controls]);
    f.render_widget(para, area);
}
