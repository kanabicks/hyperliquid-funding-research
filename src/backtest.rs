// === Backtest framework ===
//
// Симулирует "что было бы если бы бот торговал по своим signals последние N дней".
// Workflow: download historical funding → simulate hour-by-hour → report.
//
// ВАЖНЫЕ упрощения (см. WARNINGS секцию в отчёте):
//   - Считаем только funding P&L. Цена/spread/slippage/liquidation не моделируются.
//   - Volume для scoring = текущий volume из funding_snapshots (proxy, исторических нет).
//   - Tier классифицируется на основе rolling 24h окна hourly funding'а
//     (вместо watch'евского 1h на 5min snapshots — другая cadence требует адаптации).

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL};
use serde::Deserialize;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::util::format_usd;

// === CLI args ===
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliTier {
    Strong,
    Medium,
    Early,
}

// === Exit strategy ===
//
// std          — выйти если admission criteria больше не выполняется
//                (как раньше: !still_admit). Это часто выгоняет из позиции
//                на std>70 или apr ниже threshold через 1-2 часа.
// max-hold-only — выйти только по max_hold (или end_of_period). Никаких
//                signal_lost. Тестируем гипотезу: дать позиции отстояться.
// hysteresis   — entry при apr >= entry_threshold, exit при apr < exit_threshold.
//                Шире exit-buffer → больше времени для funding accruals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliExitStrategy {
    Std,
    MaxHoldOnly,
    Hysteresis,
}

fn exit_strategy_label(s: CliExitStrategy) -> &'static str {
    match s {
        CliExitStrategy::Std => "std",
        CliExitStrategy::MaxHoldOnly => "max-hold-only",
        CliExitStrategy::Hysteresis => "hysteresis",
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BacktestArgs {
    pub days: u32,
    pub capital: f64,
    pub max_positions: usize,
    pub min_tier: CliTier,
    pub hold_hours: u32,
    // === Transaction cost params (в bps, 1 bp = 0.01%) ===
    pub spread_bps: f64,
    pub taker_fee_bps: f64,
    pub maker_fee_bps: f64,
    pub use_maker: bool,
    pub slippage_bps: f64,
    /// Если задано — игнорируем tier system, входим если |funding_apr| >= threshold
    /// и std/|avg| < 0.7. Sizing в этом режиме фиксированный 10% капитала.
    pub min_funding_apr: Option<f64>,
    /// Какая логика выхода из позиции. Default = Std (текущее поведение).
    pub exit_strategy: CliExitStrategy,
    /// Только для hysteresis: вход когда |apr| >= этот порог.
    pub hysteresis_entry_apr: f64,
    /// Только для hysteresis: выход когда |apr| < этот порог.
    pub hysteresis_exit_apr: f64,
}

// Cost helpers.
//
// Taker entry: половина спреда (кросcим offer/bid) + taker fee + slippage.
// Maker entry: только maker fee (предполагаем заполнение по mid, без spread cost).
// Exit ВСЕГДА taker — потому что на signal_lost / max_hold надо выйти быстро,
// limit-ордером можно "застрять" неисполненным.
fn entry_cost_bps(args: &BacktestArgs) -> f64 {
    if args.use_maker {
        args.maker_fee_bps
    } else {
        args.spread_bps / 2.0 + args.taker_fee_bps + args.slippage_bps
    }
}

fn exit_cost_bps(args: &BacktestArgs) -> f64 {
    args.spread_bps / 2.0 + args.taker_fee_bps + args.slippage_bps
}

// Break-even APR: при каком funding APR funding_revenue за hold_hours
// >= entry_cost + exit_cost.
//
// funding_rev_per_year_$ = size * (apr_pct / 100)
// funding_rev_per_hold_$  = size * (apr_pct / 100) * (hold_hours / (365*24))
// total_cost_$           = size * total_cost_bps / 10000
//
// Уравнять и упростить:
//   apr_pct >= total_cost_bps * 365*24 / 100 / hold_hours
//          == total_cost_bps * 87.6 / hold_hours
fn break_even_apr_pct(args: &BacktestArgs) -> f64 {
    let total = entry_cost_bps(args) + exit_cost_bps(args);
    total * 87.6 / args.hold_hours as f64
}

// === Tier (адаптировано для hourly cadence) ===
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tier {
    Strong,
    Medium,
    Early,
    Weak,
}

fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::Strong => "STRONG",
        Tier::Medium => "MEDIUM",
        Tier::Early => "EARLY",
        Tier::Weak => "WEAK",
    }
}

fn tier_size_pct(t: Tier) -> f64 {
    match t {
        Tier::Strong => 0.20,
        Tier::Medium => 0.10,
        Tier::Early => 0.05,
        Tier::Weak => 0.02,
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

fn cli_tier_rank(t: CliTier) -> u8 {
    match t {
        CliTier::Strong => 3,
        CliTier::Medium => 2,
        CliTier::Early => 1,
    }
}

fn min_tier_satisfied(min: CliTier, actual: Tier) -> bool {
    tier_rank(actual) >= cli_tier_rank(min)
}

#[derive(Debug, Clone, Copy)]
enum TrendDir {
    Up,
    Down,
    Flat,
}

// === Scoring (адаптация watch.rs формулы под hourly cadence) ===
fn compute_score(apr_now: f64, avg_apr: f64, std_pct: f64, volume: f64, dir: TrendDir) -> f64 {
    let base = (apr_now.abs() / 5.0).min(50.0);
    let ratio = (volume / 100_000.0).max(1.0);
    let volume_bonus = (ratio.log10() * 10.0).min(20.0);
    let stability_bonus = (30.0 - std_pct).max(0.0);
    let direction_bonus = match (avg_apr.signum(), dir) {
        (s, TrendDir::Up) if s > 0.0 => 10.0,
        (s, TrendDir::Down) if s < 0.0 => 10.0,
        _ => 0.0,
    };
    (base + volume_bonus + stability_bonus + direction_bonus).clamp(0.0, 100.0)
}

// Tier classification для backtest (24h окно на hourly cadence):
//   STRONG: count >= 18 (~75% of 24h), std<30%, score>70  ← аналог watch'евских 6h*0.6=43@5min
//   MEDIUM: count >= 8  (~33% of 24h), std<50%, score>50
//   EARLY:  count >= 3                                  , score>40
//   WEAK:                                                  score>30
fn classify_tier(score: f64, std_pct: f64, count: usize) -> Option<Tier> {
    if std_pct > 70.0 {
        return None;
    }
    if score > 70.0 && std_pct < 30.0 && count >= 18 {
        Some(Tier::Strong)
    } else if score > 50.0 && std_pct < 50.0 && count >= 8 {
        Some(Tier::Medium)
    } else if score > 40.0 && count >= 3 {
        Some(Tier::Early)
    } else if score > 30.0 {
        Some(Tier::Weak)
    } else {
        None
    }
}

// === Position / Trade ===
#[derive(Debug, Clone, Copy)]
enum Side {
    Long,
    Short,
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    }
}

#[derive(Debug, Clone)]
struct Position {
    asset: String,
    side: Side,
    size_usd: f64,
    entry_time_sec: i64,
    entry_tier: Tier,
    accumulated_pnl: f64,        // gross funding pnl, накапливается hour-by-hour
    entry_cost: f64,             // $ paid at entry (taker или maker)
    funding_apr_at_entry: f64,   // APR в момент входа (для отчёта)
}

#[derive(Debug, Clone)]
struct Trade {
    asset: String,
    entry_tier: Tier,
    side: Side,
    size_usd: f64,
    entry_time_sec: i64,
    exit_time_sec: i64,
    // pnl ОСТАВЛЕН для совместимости с уже существующим кодом отчёта (gross funding).
    pnl: f64,
    // Новые поля:
    gross_funding_pnl: f64,
    entry_cost: f64,
    exit_cost: f64,
    net_pnl: f64,                 // gross - entry_cost - exit_cost
    funding_apr_at_entry: f64,
    funding_apr_at_exit: f64,
    hold_hours: u32,
    exit_reason: &'static str, // "max_hold" / "signal_lost" / "end_of_period"
}

// === API response ===
#[derive(Debug, Deserialize)]
struct HistEntry {
    coin: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(default)]
    premium: Option<String>,
    time: i64, // ms
}

// === Prep data (shared by run + run_sweep) ===
struct PrepData {
    pool: SqlitePool,
    volumes: HashMap<String, f64>,
    series: HashMap<String, Vec<(i64, f64)>>,
    start_ms: i64,
    end_ms: i64,
}

async fn prep_data(days: u32) -> Result<PrepData> {
    let pool = open_pool().await?;
    ensure_historical_table(&pool).await?;

    let coins = top_coins_by_current_volume(&pool, 50).await?;
    if coins.is_empty() {
        anyhow::bail!(
            "snapshots.db пустой или нет последнего snapshot — сначала запусти `collect`"
        );
    }
    println!(
        "Selected top {} coins by current volume from funding_snapshots",
        coins.len()
    );

    let now_ms = ms_now();
    let start_ms = now_ms - (days as i64) * 86_400_000;
    download_history(&pool, &coins, start_ms, now_ms).await?;

    let volumes = latest_volumes(&pool, &coins).await?;
    let series = load_series(&pool, &coins, start_ms, now_ms).await?;

    Ok(PrepData {
        pool,
        volumes,
        series,
        start_ms,
        end_ms: now_ms,
    })
}

// === Точка входа: одиночный backtest ===
pub async fn run(args: BacktestArgs) -> Result<()> {
    let p = prep_data(args.days).await?;
    let trades = simulate(&p.series, &p.volumes, &args, p.start_ms, p.end_ms);
    report(&trades, &args, p.start_ms, p.end_ms);
    p.pool.close().await;
    Ok(())
}

// === Sweep mode ===
//
// Main grid: 2 maker × 4 min_apr × 3 hold × 3 max_pos × 3 exit_strat = 216 runs.
// Hold=48 убран чтобы не раздувать (из теста — редко даёт другой результат).
// Hysteresis sub-sweep: 12 runs с разными entry/exit thresholds (только maker=true,
// max_pos=3, hold ∈ {24,48}, hyst_entry ∈ {100,150,200}, hyst_exit ∈ {25,50}).
pub async fn run_sweep(days: u32, capital: f64) -> Result<()> {
    let p = prep_data(days).await?;

    let use_makers = [false, true];
    let min_aprs = [50.0_f64, 100.0, 150.0, 200.0];
    let holds = [6_u32, 12, 24];
    let max_positions_axis = [1_usize, 3, 5];
    let exit_strats = [
        CliExitStrategy::Std,
        CliExitStrategy::MaxHoldOnly,
        CliExitStrategy::Hysteresis,
    ];
    let main_total =
        use_makers.len() * min_aprs.len() * holds.len() * max_positions_axis.len() * exit_strats.len();

    // Hysteresis sub-sweep
    let hyst_holds = [24_u32, 48];
    let hyst_entries = [100.0_f64, 150.0, 200.0];
    let hyst_exits = [25.0_f64, 50.0];
    let hyst_total = hyst_holds.len() * hyst_entries.len() * hyst_exits.len();

    let total = main_total + hyst_total;
    eprintln!(
        "Running {} parameter combinations (main {} + hysteresis sub-sweep {})...",
        total, main_total, hyst_total
    );

    let mut results: Vec<ComboResult> = Vec::with_capacity(total);
    let mut idx = 0_usize;

    // === Main grid ===
    for &use_maker in &use_makers {
        for &min_apr in &min_aprs {
            for &hold in &holds {
                for &max_pos in &max_positions_axis {
                    for &exit_strat in &exit_strats {
                        idx += 1;
                        let args = BacktestArgs {
                            days,
                            capital,
                            max_positions: max_pos,
                            min_tier: CliTier::Medium,
                            hold_hours: hold,
                            spread_bps: 20.0,
                            taker_fee_bps: 4.5,
                            maker_fee_bps: 1.5,
                            use_maker,
                            slippage_bps: 10.0,
                            min_funding_apr: Some(min_apr),
                            exit_strategy: exit_strat,
                            hysteresis_entry_apr: min_apr, // в hysteresis режиме = entry threshold
                            hysteresis_exit_apr: 50.0,     // default exit
                        };
                        let trades =
                            simulate(&p.series, &p.volumes, &args, p.start_ms, p.end_ms);
                        let combo = compute_combo_result(&trades, &args);
                        eprintln!(
                            "  combo {:>3}/{:>3}: {:<13} maker={:<5} apr={:>3}% hold={:>2}h pos={} → trades={:>3} net=${:+.2}",
                            idx, total,
                            exit_strategy_label(exit_strat),
                            use_maker, min_apr as i64, hold, max_pos,
                            combo.trades, combo.net_pnl
                        );
                        results.push(combo);
                    }
                }
            }
        }
    }

    // === Hysteresis sub-sweep ===
    eprintln!("--- Hysteresis sub-sweep (varied thresholds) ---");
    for &hold in &hyst_holds {
        for &h_entry in &hyst_entries {
            for &h_exit in &hyst_exits {
                idx += 1;
                let args = BacktestArgs {
                    days,
                    capital,
                    max_positions: 3,
                    min_tier: CliTier::Medium,
                    hold_hours: hold,
                    spread_bps: 20.0,
                    taker_fee_bps: 4.5,
                    maker_fee_bps: 1.5,
                    use_maker: true,
                    slippage_bps: 10.0,
                    min_funding_apr: None,
                    exit_strategy: CliExitStrategy::Hysteresis,
                    hysteresis_entry_apr: h_entry,
                    hysteresis_exit_apr: h_exit,
                };
                let trades = simulate(&p.series, &p.volumes, &args, p.start_ms, p.end_ms);
                let combo = compute_combo_result(&trades, &args);
                eprintln!(
                    "  combo {:>3}/{:>3}: hyst_sub    maker=true  hyst_entry={:>3}% hyst_exit={:>3}% hold={:>2}h pos=3 → trades={:>3} net=${:+.2}",
                    idx, total,
                    h_entry as i64, h_exit as i64, hold,
                    combo.trades, combo.net_pnl
                );
                results.push(combo);
            }
        }
    }

    sweep_report(&results, days, capital);

    // Verbose run with overall best.
    if let Some(best) = results.iter().max_by(|a, b| {
        a.net_pnl
            .partial_cmp(&b.net_pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        println!();
        println!("═══════════════ VERBOSE BACKTEST WITH BEST PARAMETERS ═══════════════");
        let best_args = BacktestArgs {
            days,
            capital,
            max_positions: best.max_positions,
            min_tier: CliTier::Medium,
            hold_hours: best.hold_hours,
            spread_bps: 20.0,
            taker_fee_bps: 4.5,
            maker_fee_bps: 1.5,
            use_maker: best.use_maker,
            slippage_bps: 10.0,
            min_funding_apr: if best.exit_strategy == CliExitStrategy::Hysteresis {
                None
            } else {
                Some(best.min_funding_apr)
            },
            exit_strategy: best.exit_strategy,
            hysteresis_entry_apr: best.hysteresis_entry_apr,
            hysteresis_exit_apr: best.hysteresis_exit_apr,
        };
        let trades = simulate(&p.series, &p.volumes, &best_args, p.start_ms, p.end_ms);
        report(&trades, &best_args, p.start_ms, p.end_ms);
    }

    p.pool.close().await;
    Ok(())
}

#[derive(Debug, Clone)]
struct ComboResult {
    use_maker: bool,
    min_funding_apr: f64,
    hold_hours: u32,
    max_positions: usize,
    exit_strategy: CliExitStrategy,
    hysteresis_entry_apr: f64,
    hysteresis_exit_apr: f64,
    trades: usize,
    win_pct: f64, // by net_pnl > 0
    net_pnl: f64,
    gross_pnl: f64,
    total_costs: f64,
    max_drawdown: f64,
    annualized_pct: f64,
}

fn compute_combo_result(trades: &[Trade], args: &BacktestArgs) -> ComboResult {
    let n = trades.len();
    let net_pnl: f64 = trades.iter().map(|t| t.net_pnl).sum();
    let gross_pnl: f64 = trades.iter().map(|t| t.gross_funding_pnl).sum();
    let total_costs: f64 = trades.iter().map(|t| t.entry_cost + t.exit_cost).sum();
    let wins = trades.iter().filter(|t| t.net_pnl > 0.0).count();
    let win_pct = if n > 0 {
        100.0 * wins as f64 / n as f64
    } else {
        0.0
    };

    // Max drawdown по equity curve (по net P&L, отсортированному по времени выхода).
    let mut sorted = trades.to_vec();
    sorted.sort_by_key(|t| t.exit_time_sec);
    let mut peak = 0_f64;
    let mut max_dd = 0_f64;
    let mut running = 0_f64;
    for t in &sorted {
        running += t.net_pnl;
        if running > peak {
            peak = running;
        }
        let dd = peak - running;
        if dd > max_dd {
            max_dd = dd;
        }
    }

    let annualized_pct = if args.days > 0 {
        net_pnl / args.capital * 100.0 * 365.0 / args.days as f64
    } else {
        0.0
    };

    ComboResult {
        use_maker: args.use_maker,
        min_funding_apr: args.min_funding_apr.unwrap_or(0.0),
        hold_hours: args.hold_hours,
        max_positions: args.max_positions,
        exit_strategy: args.exit_strategy,
        hysteresis_entry_apr: args.hysteresis_entry_apr,
        hysteresis_exit_apr: args.hysteresis_exit_apr,
        trades: n,
        win_pct,
        net_pnl,
        gross_pnl,
        total_costs,
        max_drawdown: max_dd,
        annualized_pct,
    }
}

fn sweep_report(results: &[ComboResult], days: u32, capital: f64) {
    println!();
    println!("═══════════════ STRATEGY PARAMETER SWEEP ═══════════════");
    println!(
        "Period: {} days | Capital: ${:.0} | Costs (taker round-trip): spread 20 + 2*taker 4.5 + 2*slip 10 = 49bps",
        days, capital
    );
    println!(
        "Costs (maker entry): spread 0 + maker 1.5 + slip 0 (entry) + 24.5bps (exit, taker) = 26bps round-trip"
    );
    println!();

    let mut sorted = results.to_vec();
    sorted.sort_by(|a, b| {
        b.net_pnl
            .partial_cmp(&a.net_pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("Top 15 combinations by net P&L (across all exit strategies):");
    print_combo_table(sorted.iter().take(15));

    println!();
    println!("Bottom 5 combinations:");
    print_combo_table(sorted.iter().rev().take(5));

    // Best per exit strategy
    println!();
    println!("Best combination per exit strategy:");
    let strats = [
        CliExitStrategy::Std,
        CliExitStrategy::MaxHoldOnly,
        CliExitStrategy::Hysteresis,
    ];
    let bests: Vec<&ComboResult> = strats
        .iter()
        .filter_map(|&s| sorted.iter().find(|c| c.exit_strategy == s))
        .collect();
    print_combo_table(bests.into_iter());

    // === Insights ===
    println!();
    println!("═══════════════ INSIGHTS ═══════════════");

    if let Some(best) = sorted.first() {
        let entry_desc = match best.exit_strategy {
            CliExitStrategy::Hysteresis => format!(
                "hyst_entry={:.0}% hyst_exit={:.0}%",
                best.hysteresis_entry_apr, best.hysteresis_exit_apr
            ),
            _ => format!("min_apr={:.0}%", best.min_funding_apr),
        };
        println!(
            "Best: strat={} | maker={} | {} | hold={}h | max_pos={}",
            exit_strategy_label(best.exit_strategy),
            best.use_maker,
            entry_desc,
            best.hold_hours,
            best.max_positions
        );
        println!(
            "      Net ${:+.2} | annualized {:+.1}% | {} trades | {:.0}% win | maxDD ${:.2}",
            best.net_pnl,
            best.annualized_pct,
            best.trades,
            best.win_pct,
            best.max_drawdown
        );
    }

    // Profitable counts per strategy
    let count_profitable = |s: CliExitStrategy| -> (usize, usize) {
        let total = sorted.iter().filter(|c| c.exit_strategy == s).count();
        let prof = sorted.iter().filter(|c| c.exit_strategy == s && c.net_pnl > 0.0).count();
        (prof, total)
    };
    for s in &strats {
        let (p, total) = count_profitable(*s);
        println!(
            "Profitable combos for {}: {}/{}",
            exit_strategy_label(*s),
            p,
            total
        );
    }

    let lowest_maker = sorted
        .iter()
        .filter(|c| c.use_maker && c.net_pnl > 0.0)
        .map(|c| c.min_funding_apr)
        .fold(f64::INFINITY, f64::min);
    if lowest_maker.is_finite() {
        println!(
            "Lowest profitable APR threshold (maker entry): {:.0}%",
            lowest_maker
        );
    } else {
        println!("Lowest profitable APR threshold (maker entry): no profitable maker combo found");
    }

    let lowest_taker = sorted
        .iter()
        .filter(|c| !c.use_maker && c.net_pnl > 0.0)
        .map(|c| c.min_funding_apr)
        .fold(f64::INFINITY, f64::min);
    if lowest_taker.is_finite() {
        println!(
            "Lowest profitable APR threshold (taker entry): {:.0}%",
            lowest_taker
        );
    } else {
        println!("Lowest profitable APR threshold (taker entry): no profitable taker combo found");
    }

    // Группировка по hold_hours / max_positions: avg net P&L.
    let avg_by = |key_fn: &dyn Fn(&ComboResult) -> i64| -> Vec<(i64, f64, usize)> {
        let mut map: HashMap<i64, Vec<f64>> = HashMap::new();
        for c in &sorted {
            map.entry(key_fn(c)).or_default().push(c.net_pnl);
        }
        let mut v: Vec<(i64, f64, usize)> = map
            .into_iter()
            .map(|(k, vals)| {
                let n = vals.len();
                let avg = vals.iter().sum::<f64>() / n as f64;
                (k, avg, n)
            })
            .collect();
        v.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    };

    let by_hold = avg_by(&|c| c.hold_hours as i64);
    println!("Avg net P&L by hold_hours (across all maker/apr/pos combos):");
    for (h, avg, n) in &by_hold {
        println!("  {}h: ${:+.2}  ({} combos)", h, avg, n);
    }

    let by_pos = avg_by(&|c| c.max_positions as i64);
    println!("Avg net P&L by max_positions:");
    for (p, avg, n) in &by_pos {
        println!("  {}: ${:+.2}  ({} combos)", p, avg, n);
    }

    // === Warnings ===
    println!();
    println!("═══════════════ WARNINGS ═══════════════");
    println!(
        "- Sweep на {} днях статистически слаб — для надёжности нужно 90+ дней истории.",
        days
    );
    println!("- Maker entry assumed instant fill at mid-price — в реальности часть orders не заполнится.");
    println!("- Не учтено: liquidation risk, price movement, корреляция между positions.");
    println!("- Sizing в APR mode фиксированный 10% капитала (не зависит от tier) — для чистоты сравнения.");
}

fn print_combo_table<'a, I: Iterator<Item = &'a ComboResult>>(rows: I) {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL);
    t.set_header(vec![
        Cell::new("Strategy"),
        Cell::new("Maker?"),
        Cell::new("Entry APR%"),  // для hysteresis = hyst_entry, иначе = min_funding_apr
        Cell::new("Exit APR%"),   // для hysteresis = hyst_exit, иначе "—"
        Cell::new("Hold h"),
        Cell::new("MaxPos"),
        Cell::new("Trades"),
        Cell::new("Win%"),
        Cell::new("Net $"),
        Cell::new("Annualized"),
        Cell::new("MaxDD $"),
    ]);
    for c in rows {
        let net_color = if c.net_pnl >= 0.0 {
            Color::Green
        } else {
            Color::Red
        };
        let strat_color = match c.exit_strategy {
            CliExitStrategy::Std => Color::Grey,
            CliExitStrategy::MaxHoldOnly => Color::Cyan,
            CliExitStrategy::Hysteresis => Color::Yellow,
        };
        let (entry_apr, exit_apr) = match c.exit_strategy {
            CliExitStrategy::Hysteresis => (
                format!("{:.0}", c.hysteresis_entry_apr),
                format!("{:.0}", c.hysteresis_exit_apr),
            ),
            _ => (format!("{:.0}", c.min_funding_apr), "—".to_string()),
        };
        t.add_row(vec![
            Cell::new(exit_strategy_label(c.exit_strategy)).fg(strat_color),
            Cell::new(if c.use_maker { "TRUE" } else { "FALSE" }),
            Cell::new(entry_apr),
            Cell::new(exit_apr),
            Cell::new(c.hold_hours),
            Cell::new(c.max_positions),
            Cell::new(c.trades),
            Cell::new(format!("{:.0}%", c.win_pct)),
            Cell::new(format!("${:+.2}", c.net_pnl)).fg(net_color),
            Cell::new(format!("{:+.1}%", c.annualized_pct)),
            Cell::new(format!("${:.2}", c.max_drawdown)),
        ]);
    }
    println!("{t}");
}

fn ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn open_pool() -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite://./data/snapshots.db")
        .context("parse sqlite URL")?
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .context("open snapshots.db")?;
    Ok(pool)
}

async fn ensure_historical_table(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS historical_funding (
            coin TEXT NOT NULL,
            time_ms INTEGER NOT NULL,
            funding_rate REAL NOT NULL,
            premium REAL,
            PRIMARY KEY (coin, time_ms)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create historical_funding")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_hist_coin_time \
         ON historical_funding(coin, time_ms DESC)",
    )
    .execute(pool)
    .await
    .context("create idx_hist_coin_time")?;

    Ok(())
}

async fn top_coins_by_current_volume(pool: &SqlitePool, limit: usize) -> Result<Vec<String>> {
    // Берём последний по timestamp snapshot, в нём top N по day_volume_usd.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT asset FROM funding_snapshots \
         WHERE timestamp = (SELECT MAX(timestamp) FROM funding_snapshots) \
         ORDER BY day_volume_usd DESC \
         LIMIT ?",
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .context("top coins query")?;
    Ok(rows.into_iter().map(|(a,)| a).collect())
}

async fn latest_volumes(pool: &SqlitePool, coins: &[String]) -> Result<HashMap<String, f64>> {
    // Latest snapshot per coin → volume.
    // Простой подход: один WHERE asset IN (...) к последнему snapshot.
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT asset, day_volume_usd FROM funding_snapshots \
         WHERE timestamp = (SELECT MAX(timestamp) FROM funding_snapshots)",
    )
    .fetch_all(pool)
    .await
    .context("latest volumes query")?;

    let coin_set: std::collections::HashSet<&String> = coins.iter().collect();
    let mut map = HashMap::new();
    for (asset, vol) in rows {
        if coin_set.contains(&asset) {
            map.insert(asset, vol);
        }
    }
    Ok(map)
}

// === Download phase ===
//
// Per coin: смотрим max stored time_ms, докачиваем delta. INSERT OR IGNORE
// делает повторный фул-fetch безопасным (UNIQUE constraint на PRIMARY KEY).
//
// Hyperliquid возвращает max ~500 entries на вызов; пагинация: следующий вызов
// startTime = последнего полученного time + 1.
//
// Rate limit: sleep 200ms между запросами (5 req/s).
// Retry: exponential backoff 1s/2s/4s, max 3 attempts.
async fn download_history(
    pool: &SqlitePool,
    coins: &[String],
    start_ms: i64,
    end_ms: i64,
) -> Result<()> {
    let client = reqwest::Client::new();
    println!(
        "Downloading historical funding: {} coins, period {} days...",
        coins.len(),
        (end_ms - start_ms) / 86_400_000
    );

    let mut total_inserted = 0usize;
    let mut total_skipped = 0usize;

    for (i, coin) in coins.iter().enumerate() {
        // Проверяем что у нас уже есть.
        let stored_max: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(time_ms) FROM historical_funding WHERE coin = ?",
        )
        .bind(coin)
        .fetch_one(pool)
        .await
        .ok()
        .flatten();

        let fetch_start = stored_max.map(|m| m + 1).unwrap_or(start_ms).max(start_ms);
        if fetch_start >= end_ms {
            total_skipped += 1;
            continue;
        }

        match download_coin_paginated(&client, coin, fetch_start, end_ms).await {
            Ok(entries) => {
                let n = entries.len();
                if n > 0 {
                    let mut tx = pool.begin().await.context("begin tx")?;
                    for e in &entries {
                        let rate: f64 = e.funding_rate.parse().unwrap_or(0.0);
                        let premium: Option<f64> =
                            e.premium.as_ref().and_then(|s| s.parse().ok());
                        sqlx::query(
                            "INSERT OR IGNORE INTO historical_funding \
                             (coin, time_ms, funding_rate, premium) \
                             VALUES (?, ?, ?, ?)",
                        )
                        .bind(&e.coin)
                        .bind(e.time)
                        .bind(rate)
                        .bind(premium)
                        .execute(&mut *tx)
                        .await
                        .context("insert hist")?;
                    }
                    tx.commit().await.context("commit hist")?;
                }
                total_inserted += n;
                if (i + 1) % 10 == 0 {
                    println!(
                        "  ... {}/{} coins, +{} entries so far",
                        i + 1,
                        coins.len(),
                        total_inserted
                    );
                }
            }
            Err(e) => {
                eprintln!("  WARN: failed to download {}: {:#}", coin, e);
            }
        }
    }

    println!(
        "Download done: {} new entries inserted, {} coins already up-to-date",
        total_inserted, total_skipped
    );
    Ok(())
}

async fn download_coin_paginated(
    client: &reqwest::Client,
    coin: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<HistEntry>> {
    let mut all = Vec::new();
    let mut cursor = start_ms;

    loop {
        let chunk = fetch_chunk_with_retry(client, coin, cursor, end_ms).await?;
        let n = chunk.len();
        if n == 0 {
            break;
        }
        // Самый поздний time из chunk'а.
        let last_time = chunk.iter().map(|e| e.time).max().unwrap();
        all.extend(chunk);
        if n < 500 {
            break;
        }
        cursor = last_time + 1;
        if cursor >= end_ms {
            break;
        }
        // Rate limit между чанками тоже.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Rate limit между coins.
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(all)
}

async fn fetch_chunk_with_retry(
    client: &reqwest::Client,
    coin: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<HistEntry>> {
    let mut delay_ms: u64 = 1000;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3 {
        match fetch_chunk(client, coin, start_ms, end_ms).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                eprintln!(
                    "  retry {}/3 for {}: {:#}",
                    attempt + 1,
                    coin,
                    e
                );
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry exhausted")))
}

async fn fetch_chunk(
    client: &reqwest::Client,
    coin: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<HistEntry>> {
    let body = serde_json::json!({
        "type": "fundingHistory",
        "coin": coin,
        "startTime": start_ms,
        "endTime": end_ms,
    });
    let resp = client
        .post("https://api.hyperliquid.xyz/info")
        .json(&body)
        .send()
        .await
        .context("http send")?
        .error_for_status()
        .context("http status")?;
    let entries: Vec<HistEntry> = resp.json().await.context("json parse")?;
    Ok(entries)
}

// === Load series in memory ===
//
// HashMap<coin, Vec<(time_ms, hourly_funding_rate)>> отсортированный по времени.
// Hourly funding rate как пришёл от API (fractional, e.g. 0.0001).
// При расчёте APR умножим на 24*365*100.
async fn load_series(
    pool: &SqlitePool,
    coins: &[String],
    start_ms: i64,
    end_ms: i64,
) -> Result<HashMap<String, Vec<(i64, f64)>>> {
    let rows: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT coin, time_ms, funding_rate FROM historical_funding \
         WHERE time_ms BETWEEN ? AND ? \
         ORDER BY coin, time_ms",
    )
    .bind(start_ms)
    .bind(end_ms)
    .fetch_all(pool)
    .await
    .context("load series")?;

    let coin_set: std::collections::HashSet<&String> = coins.iter().collect();
    let mut map: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    for (coin, time, rate) in rows {
        if coin_set.contains(&coin) {
            map.entry(coin).or_default().push((time, rate));
        }
    }
    Ok(map)
}

// === Simulate ===
//
// Шагаем по времени hour-by-hour. Сначала warmup (24h), чтобы было из чего
// считать avg/std для tier classification.
fn simulate(
    series: &HashMap<String, Vec<(i64, f64)>>,
    volumes: &HashMap<String, f64>,
    args: &BacktestArgs,
    start_ms: i64,
    end_ms: i64,
) -> Vec<Trade> {
    const HOUR_MS: i64 = 3_600_000;
    const LOOKBACK_MS: i64 = 24 * HOUR_MS;

    // Bucket-aligned start/end (округляем к часу).
    let warmup_until = start_ms + LOOKBACK_MS;
    let sim_start = (warmup_until / HOUR_MS) * HOUR_MS;
    let sim_end = (end_ms / HOUR_MS) * HOUR_MS;

    let mut positions: Vec<Position> = Vec::new();
    let mut trades: Vec<Trade> = Vec::new();

    let mut t = sim_start;
    while t <= sim_end {
        let t_sec = t / 1000;

        // 1) Compute snap для каждой монеты на момент t (окно [t-24h, t]).
        let mut snap_at_t: HashMap<String, AssetSnap> = HashMap::new();
        for (coin, ser) in series {
            if let Some(snap) = state_at(
                ser,
                t,
                LOOKBACK_MS,
                volumes.get(coin).copied().unwrap_or(0.0),
            ) {
                snap_at_t.insert(coin.clone(), snap);
            }
        }

        // 2) Аккумуляция P&L для открытых позиций — ищем funding rate для текущего часа.
        for pos in positions.iter_mut() {
            if let Some(ser) = series.get(&pos.asset) {
                // Функдинг для часа t — entry с самым большим time <= t (но > t - HOUR_MS).
                let bucket_lower = t - HOUR_MS;
                let bucket_upper = t;
                if let Some(rate) = ser.iter().rev()
                    .find(|(time, _)| *time > bucket_lower && *time <= bucket_upper)
                    .map(|(_, r)| *r)
                {
                    // P&L: LONG получает -rate*size, SHORT получает +rate*size.
                    let pnl_delta = match pos.side {
                        Side::Long => -rate * pos.size_usd,
                        Side::Short => rate * pos.size_usd,
                    };
                    pos.accumulated_pnl += pnl_delta;
                }
            }
        }

        // 3) Закрытие: max hold или signal lost (admission больше не выполняется).
        let mut still_open: Vec<Position> = Vec::with_capacity(positions.len());
        for pos in positions.drain(..) {
            let age_h = ((t_sec - pos.entry_time_sec) / 3600) as u32;
            let cur_snap = snap_at_t.get(&pos.asset);
            let close_reason: Option<&'static str> = if age_h >= args.hold_hours {
                Some("max_hold")
            } else if !still_admit(args, cur_snap) {
                Some("signal_lost")
            } else {
                None
            };

            if let Some(reason) = close_reason {
                let exit_cost = pos.size_usd * exit_cost_bps(args) / 10000.0;
                let funding_apr_at_exit = cur_snap
                    .map(|s| s.apr_now)
                    .or_else(|| series.get(&pos.asset).and_then(|s| apr_at(s, t)))
                    .unwrap_or(pos.funding_apr_at_entry);
                let gross = pos.accumulated_pnl;
                let net = gross - pos.entry_cost - exit_cost;
                trades.push(Trade {
                    asset: pos.asset.clone(),
                    entry_tier: pos.entry_tier,
                    side: pos.side,
                    size_usd: pos.size_usd,
                    entry_time_sec: pos.entry_time_sec,
                    exit_time_sec: t_sec,
                    pnl: gross,
                    gross_funding_pnl: gross,
                    entry_cost: pos.entry_cost,
                    exit_cost,
                    net_pnl: net,
                    funding_apr_at_entry: pos.funding_apr_at_entry,
                    funding_apr_at_exit,
                    hold_hours: age_h.max(1),
                    exit_reason: reason,
                });
            } else {
                still_open.push(pos);
            }
        }
        positions = still_open;

        // 4) Открытие новых позиций.
        //   - tier mode: сортировка tier_rank desc → |apr| desc
        //   - APR mode:  сортировка |apr| desc (tier игнорится для admission)
        let mut candidates: Vec<(&String, &AssetSnap)> = snap_at_t
            .iter()
            .filter(|(coin, snap)| {
                admit(args, snap) && !positions.iter().any(|p| &p.asset == *coin)
            })
            .collect();
        candidates.sort_by(|a, b| {
            // hysteresis и APR-mode: sort по |apr| desc.
            // tier-mode: tier_rank desc, потом |apr| desc.
            let apr_only = args.exit_strategy == CliExitStrategy::Hysteresis
                || args.min_funding_apr.is_some();
            if apr_only {
                b.1.apr_now
                    .abs()
                    .partial_cmp(&a.1.apr_now.abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                let a_rank = a.1.natural_tier.map(tier_rank).unwrap_or(0);
                let b_rank = b.1.natural_tier.map(tier_rank).unwrap_or(0);
                b_rank.cmp(&a_rank).then(
                    b.1.apr_now
                        .abs()
                        .partial_cmp(&a.1.apr_now.abs())
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            }
        });

        for (coin, snap) in candidates {
            if positions.len() >= args.max_positions {
                break;
            }
            let side = if snap.apr_now > 0.0 {
                Side::Short
            } else {
                Side::Long
            };
            // Sizing:
            //   - tier mode: по natural_tier (как раньше)
            //   - APR mode и hysteresis: фиксированные 10% капитала
            let entry_tier_for_sizing = snap.natural_tier.unwrap_or(Tier::Weak);
            let apr_or_hyst = args.min_funding_apr.is_some()
                || args.exit_strategy == CliExitStrategy::Hysteresis;
            let size = if apr_or_hyst {
                args.capital * 0.10
            } else {
                args.capital * tier_size_pct(entry_tier_for_sizing)
            };
            let entry_cost = size * entry_cost_bps(args) / 10000.0;
            positions.push(Position {
                asset: coin.clone(),
                side,
                size_usd: size,
                entry_time_sec: t_sec,
                entry_tier: entry_tier_for_sizing,
                accumulated_pnl: 0.0,
                entry_cost,
                funding_apr_at_entry: snap.apr_now,
            });
        }

        t += HOUR_MS;
    }

    // Закрыть всё что осталось — exit_reason = "end_of_period".
    let final_t_sec = sim_end / 1000;
    let final_t_ms = sim_end;
    for pos in positions {
        let age_h = ((final_t_sec - pos.entry_time_sec) / 3600).max(1) as u32;
        let exit_cost = pos.size_usd * exit_cost_bps(args) / 10000.0;
        let funding_apr_at_exit = series
            .get(&pos.asset)
            .and_then(|s| apr_at(s, final_t_ms))
            .unwrap_or(pos.funding_apr_at_entry);
        let gross = pos.accumulated_pnl;
        let net = gross - pos.entry_cost - exit_cost;
        trades.push(Trade {
            asset: pos.asset,
            entry_tier: pos.entry_tier,
            side: pos.side,
            size_usd: pos.size_usd,
            entry_time_sec: pos.entry_time_sec,
            exit_time_sec: final_t_sec,
            pnl: gross,
            gross_funding_pnl: gross,
            entry_cost: pos.entry_cost,
            exit_cost,
            net_pnl: net,
            funding_apr_at_entry: pos.funding_apr_at_entry,
            funding_apr_at_exit,
            hold_hours: age_h,
            exit_reason: "end_of_period",
        });
    }

    trades
}

fn print_trade_detail(rank: usize, t: &Trade) {
    let entry_dt = DateTime::<Utc>::from_timestamp(t.entry_time_sec, 0)
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string());
    let exit_dt = DateTime::<Utc>::from_timestamp(t.exit_time_sec, 0)
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string());
    println!(
        "{}. {} {} entry {} exit {} ({}h, size ${:.0})",
        rank,
        t.asset,
        side_str(t.side),
        entry_dt,
        exit_dt,
        t.hold_hours,
        t.size_usd,
    );
    println!(
        "   Entry funding: {:+.0}% APR, Exit: {:+.0}% APR",
        t.funding_apr_at_entry, t.funding_apr_at_exit
    );
    println!(
        "   Gross funding: ${:+.3}, Costs: -${:.3} (entry {:.3}+exit {:.3}), Net: ${:+.3}",
        t.gross_funding_pnl,
        t.entry_cost + t.exit_cost,
        t.entry_cost,
        t.exit_cost,
        t.net_pnl,
    );
    println!(
        "   Reason exit: {}, entry tier: {}",
        t.exit_reason,
        tier_label(t.entry_tier)
    );
}

// Helper: latest APR (annualized %) at or before t_ms.
fn apr_at(series: &[(i64, f64)], t_ms: i64) -> Option<f64> {
    let idx = series.partition_point(|(t, _)| *t <= t_ms);
    if idx == 0 {
        return None;
    }
    let rate = series[idx - 1].1;
    Some(rate * 24.0 * 365.0 * 100.0)
}

// AssetSnap — снимок состояния одного coin'а на момент t.
// Возвращается всегда когда есть хоть одна точка в окне; admit-логика
// решается в simulate (зависит от tier vs min-funding-apr режима).
#[derive(Debug, Clone, Copy)]
struct AssetSnap {
    apr_now: f64,
    std_pct: f64,
    natural_tier: Option<Tier>, // None если score<30 или std>70
    count: usize,
}

fn state_at(
    series: &[(i64, f64)],
    t_ms: i64,
    lookback_ms: i64,
    volume: f64,
) -> Option<AssetSnap> {
    let lo = t_ms - lookback_ms;
    let start_idx = series.partition_point(|(time, _)| *time <= lo);
    let end_idx = series.partition_point(|(time, _)| *time <= t_ms);
    if start_idx >= end_idx {
        return None;
    }
    let window = &series[start_idx..end_idx];

    let aprs: Vec<f64> = window.iter().map(|(_, r)| r * 24.0 * 365.0 * 100.0).collect();
    let n = aprs.len() as f64;
    let apr_now = *aprs.last().unwrap();

    let sum: f64 = aprs.iter().sum();
    let sum_sq: f64 = aprs.iter().map(|a| a * a).sum();
    let avg = sum / n;
    let var = (sum_sq / n - avg * avg).max(0.0);
    let std = var.sqrt();
    let std_pct = if avg.abs() > 1e-9 {
        100.0 * std / avg.abs()
    } else {
        0.0
    };

    let direction = if aprs.len() >= 3 {
        let v_old = aprs[aprs.len() - 3];
        let v_mid = aprs[aprs.len() - 2];
        let v_new = aprs[aprs.len() - 1];
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

    let score = compute_score(apr_now, avg, std_pct, volume, direction);
    let natural_tier = classify_tier(score, std_pct, aprs.len());
    Some(AssetSnap {
        apr_now,
        std_pct,
        natural_tier,
        count: aprs.len(),
    })
}

// Admission check: можно ли открыть позицию по этому ассету сейчас.
//
// Hysteresis-mode: вход по hysteresis_entry_apr, игнорируя min_funding_apr/tier.
// Остальные strategies: по min_funding_apr (если задан) или по tier (иначе).
fn admit(args: &BacktestArgs, snap: &AssetSnap) -> bool {
    if args.exit_strategy == CliExitStrategy::Hysteresis {
        return snap.apr_now.abs() >= args.hysteresis_entry_apr && snap.std_pct < 70.0;
    }
    match args.min_funding_apr {
        Some(threshold) => snap.apr_now.abs() >= threshold && snap.std_pct < 70.0,
        None => snap
            .natural_tier
            .map(|t| min_tier_satisfied(args.min_tier, t))
            .unwrap_or(false),
    }
}

// Hold check: должна ли позиция продолжать существовать? false → signal_lost.
//
// max-hold-only: всегда true (только max_hold выходит из позиции).
// hysteresis:    держим пока |apr| >= hysteresis_exit_apr (нет std-проверки).
// std:           старое поведение (admission criteria continued).
fn still_admit(args: &BacktestArgs, snap: Option<&AssetSnap>) -> bool {
    match args.exit_strategy {
        CliExitStrategy::MaxHoldOnly => true,
        CliExitStrategy::Hysteresis => match snap {
            None => false,
            Some(s) => s.apr_now.abs() >= args.hysteresis_exit_apr,
        },
        CliExitStrategy::Std => match (args.min_funding_apr, snap) {
            (_, None) => false,
            (Some(threshold), Some(s)) => s.apr_now.abs() >= threshold && s.std_pct < 70.0,
            (None, Some(s)) => matches!(
                s.natural_tier,
                Some(Tier::Strong) | Some(Tier::Medium) | Some(Tier::Early)
            ),
        },
    }
}

// === Report ===

fn report(trades: &[Trade], args: &BacktestArgs, start_ms: i64, end_ms: i64) {
    let start_dt = DateTime::<Utc>::from_timestamp(start_ms / 1000, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let end_dt = DateTime::<Utc>::from_timestamp(end_ms / 1000, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    println!();
    println!("═══════════════ BACKTEST SUMMARY ═══════════════");
    println!(
        "Period: {} to {} ({} days)",
        start_dt, end_dt, args.days
    );
    println!("Initial capital: ${:.0}", args.capital);
    println!("Max concurrent positions: {}", args.max_positions);
    println!(
        "Min tier: {}",
        match args.min_tier {
            CliTier::Strong => "STRONG",
            CliTier::Medium => "MEDIUM",
            CliTier::Early => "EARLY",
        }
    );
    println!("Hold hours: {}", args.hold_hours);

    if trades.is_empty() {
        println!();
        println!("No trades executed. Possible reasons:");
        println!("  - No coin reached min_tier during the period");
        println!(
            "  - Not enough historical data (need >= 24h warm-up before first decision)"
        );
        println!("  - Try lowering --min-tier to EARLY or extending --days");
        return;
    }

    let total = trades.len();
    let wins: Vec<&Trade> = trades.iter().filter(|t| t.pnl > 0.0).collect();
    let losses: Vec<&Trade> = trades.iter().filter(|t| t.pnl < 0.0).collect();
    let total_pnl: f64 = trades.iter().map(|t| t.pnl).sum();
    let avg_pnl = total_pnl / total as f64;
    let best = trades
        .iter()
        .max_by(|a, b| a.pnl.partial_cmp(&b.pnl).unwrap())
        .unwrap();
    let worst = trades
        .iter()
        .min_by(|a, b| a.pnl.partial_cmp(&b.pnl).unwrap())
        .unwrap();

    let win_pct = wins.len() as f64 / total as f64 * 100.0;
    let loss_pct = losses.len() as f64 / total as f64 * 100.0;

    let pct_return = total_pnl / args.capital * 100.0;
    let annualized = pct_return * 365.0 / args.days as f64;

    // Sharpe-ish: mean_daily_pnl / std_daily_pnl. Bucket by day.
    let mut daily_pnl: BTreeMap<i64, f64> = BTreeMap::new();
    for t in trades {
        let day = t.exit_time_sec / 86_400;
        *daily_pnl.entry(day).or_insert(0.0) += t.pnl;
    }
    // Заполнить пропущенные дни нулями для honest std calc.
    let day_start = start_ms / 1000 / 86_400;
    let day_end = end_ms / 1000 / 86_400;
    for d in day_start..=day_end {
        daily_pnl.entry(d).or_insert(0.0);
    }
    let daily_values: Vec<f64> = daily_pnl.values().copied().collect();
    let mean_daily = daily_values.iter().sum::<f64>() / daily_values.len() as f64;
    let var_daily = daily_values.iter().map(|v| (v - mean_daily).powi(2)).sum::<f64>()
        / daily_values.len() as f64;
    let std_daily = var_daily.sqrt();
    let sharpe = if std_daily > 1e-9 {
        mean_daily / std_daily
    } else {
        0.0
    };

    // Max drawdown: equity curve, peak-to-trough.
    let mut equity: Vec<(i64, f64)> = Vec::new();
    let mut sorted_trades = trades.to_vec();
    sorted_trades.sort_by_key(|t| t.exit_time_sec);
    let mut running = 0.0;
    for t in &sorted_trades {
        running += t.pnl;
        equity.push((t.exit_time_sec, running));
    }
    let mut peak = 0.0_f64;
    let mut max_dd = 0.0_f64;
    for (_, eq) in &equity {
        if *eq > peak {
            peak = *eq;
        }
        let dd = peak - eq;
        if dd > max_dd {
            max_dd = dd;
        }
    }

    println!();
    println!("═══════════════ RESULTS (gross funding, before costs) ═══════════════");
    println!("Total trades: {}", total);
    println!("Winning: {} ({:.0}%)", wins.len(), win_pct);
    println!("Losing:  {} ({:.0}%)", losses.len(), loss_pct);
    println!("Avg P&L per trade (gross): ${:+.2}", avg_pnl);
    println!(
        "Best trade (gross):  ${:+.2} ({}, {}h hold, side={})",
        best.pnl,
        best.asset,
        best.hold_hours,
        side_str(best.side)
    );
    println!(
        "Worst trade (gross): ${:+.2} ({}, {}h hold, exit_reason={})",
        worst.pnl, worst.asset, worst.hold_hours, worst.exit_reason
    );
    println!();
    println!(
        "Total P&L (gross): ${:+.2} ({:+.2}% over {} days, ~{:+.0}% annualized)",
        total_pnl, pct_return, args.days, annualized
    );
    println!(
        "Sharpe-ish ratio (daily): {:.2}  (mean_daily=${:.2}, std_daily=${:.2})",
        sharpe, mean_daily, std_daily
    );
    println!("Max drawdown (gross): ${:+.2} (peak to trough)", -max_dd);

    // === COST BREAKDOWN ===
    let total_entry_cost: f64 = trades.iter().map(|t| t.entry_cost).sum();
    let total_exit_cost: f64 = trades.iter().map(|t| t.exit_cost).sum();
    let total_costs = total_entry_cost + total_exit_cost;
    let net_total: f64 = trades.iter().map(|t| t.net_pnl).sum();
    let cost_pct_of_gross = if total_pnl.abs() > 1e-9 {
        100.0 * total_costs / total_pnl.abs()
    } else {
        0.0
    };
    let cost_to_profit_ratio = if total_pnl > 1e-9 {
        100.0 * total_costs / total_pnl
    } else {
        f64::INFINITY
    };

    println!();
    println!("═══════════════ COST BREAKDOWN ═══════════════");
    println!(
        "Entry cost per trade: {:.1} bps ({})",
        entry_cost_bps(args),
        if args.use_maker {
            "maker (mid fill assumed)"
        } else {
            "taker = spread/2 + taker_fee + slippage"
        }
    );
    println!(
        "Exit cost per trade:  {:.1} bps (taker = spread/2 + taker_fee + slippage)",
        exit_cost_bps(args)
    );
    println!("Total entry costs: ${:.2}", total_entry_cost);
    println!("Total exit costs:  ${:.2}", total_exit_cost);
    println!(
        "Total costs: ${:.2} ({:.1}% of gross P&L)",
        total_costs, cost_pct_of_gross
    );
    println!("Gross P&L (before costs): ${:+.2}", total_pnl);
    println!("Net P&L (after costs):    ${:+.2}", net_total);
    if cost_to_profit_ratio.is_finite() {
        println!(
            "Cost-to-profit ratio: {:.1}% {}",
            cost_to_profit_ratio,
            if cost_to_profit_ratio > 100.0 {
                "(>100% — стратегия структурно убыточна)"
            } else {
                ""
            }
        );
    } else {
        println!("Cost-to-profit ratio: n/a (gross P&L ≤ 0)");
    }

    // === COST-ADJUSTED METRICS ===
    let net_pct = net_total / args.capital * 100.0;
    let net_annualized = net_pct * 365.0 / args.days as f64;
    let break_even = break_even_apr_pct(args);
    let avg_funding_at_entry: f64 =
        trades.iter().map(|t| t.funding_apr_at_entry.abs()).sum::<f64>() / total as f64;

    println!();
    println!("═══════════════ COST-ADJUSTED METRICS ═══════════════");
    println!(
        "Annualized return (gross): {:+.1}%",
        annualized
    );
    println!(
        "Annualized return (net of costs): {:+.1}%",
        net_annualized
    );
    println!(
        "Break-even funding APR: {:.0}% (минимум |APR| при котором funding покрывает costs за {}h hold)",
        break_even, args.hold_hours
    );
    println!(
        "Average |funding APR| at entry: {:.0}%",
        avg_funding_at_entry
    );
    if avg_funding_at_entry < break_even {
        println!(
            "  ⚠ Avg entry APR ({:.0}%) < break-even ({:.0}%) — стратегия структурно убыточна с этими costs.",
            avg_funding_at_entry, break_even
        );
    } else {
        println!(
            "  ✓ Avg entry APR ({:.0}%) >= break-even ({:.0}%) — стратегия выживает (по среднему).",
            avg_funding_at_entry, break_even
        );
    }

    // === TOP 5 BEST / WORST по NET P&L ===
    let mut by_net: Vec<&Trade> = trades.iter().collect();
    by_net.sort_by(|a, b| b.net_pnl.partial_cmp(&a.net_pnl).unwrap_or(std::cmp::Ordering::Equal));

    println!();
    println!("═══════════════ TOP 5 BEST TRADES (by net P&L) ═══════════════");
    for (i, t) in by_net.iter().take(5).enumerate() {
        print_trade_detail(i + 1, t);
    }

    println!();
    println!("═══════════════ TOP 5 WORST TRADES (by net P&L) ═══════════════");
    for (i, t) in by_net.iter().rev().take(5).enumerate() {
        print_trade_detail(i + 1, t);
    }

    // === Per-asset breakdown ===
    println!();
    println!("═══════════════ PER-ASSET BREAKDOWN ═══════════════");
    let mut by_asset: HashMap<String, Vec<&Trade>> = HashMap::new();
    for t in trades {
        by_asset.entry(t.asset.clone()).or_default().push(t);
    }
    let mut asset_stats: Vec<(String, usize, f64, f64, f64)> = by_asset
        .into_iter()
        .map(|(asset, ts)| {
            let n = ts.len();
            let wins = ts.iter().filter(|t| t.pnl > 0.0).count();
            let win_pct = wins as f64 / n as f64 * 100.0;
            let total_pnl: f64 = ts.iter().map(|t| t.pnl).sum();
            let avg = total_pnl / n as f64;
            (asset, n, win_pct, total_pnl, avg)
        })
        .collect();
    asset_stats.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    // Net stats per asset (для дополнительной колонки).
    let mut by_asset_trades: HashMap<String, Vec<&Trade>> = HashMap::new();
    for t in trades {
        by_asset_trades.entry(t.asset.clone()).or_default().push(t);
    }
    let net_per_asset: HashMap<String, f64> = by_asset_trades
        .iter()
        .map(|(a, ts)| (a.clone(), ts.iter().map(|t| t.net_pnl).sum()))
        .collect();

    let mut t_assets = Table::new();
    t_assets.load_preset(UTF8_FULL);
    t_assets.set_header(vec![
        Cell::new("Asset"),
        Cell::new("Trades"),
        Cell::new("Win%"),
        Cell::new("Gross P&L"),
        Cell::new("Avg/Trade"),
        Cell::new("Net P&L"),
    ]);
    for (asset, n, win_pct, total, avg) in asset_stats.iter().take(20) {
        let gross_color = if *total >= 0.0 { Color::Green } else { Color::Red };
        let net = net_per_asset.get(asset).copied().unwrap_or(0.0);
        let net_color = if net >= 0.0 { Color::Green } else { Color::Red };
        t_assets.add_row(vec![
            Cell::new(asset).fg(Color::Cyan),
            Cell::new(n),
            Cell::new(format!("{:.0}%", win_pct)),
            Cell::new(format!("${:+.2}", total)).fg(gross_color),
            Cell::new(format!("${:+.2}", avg)),
            Cell::new(format!("${:+.2}", net)).fg(net_color),
        ]);
    }
    println!("{t_assets}");

    // === Per-tier breakdown ===
    println!();
    println!("═══════════════ TIER PERFORMANCE ═══════════════");
    let mut by_tier: HashMap<Tier, Vec<&Trade>> = HashMap::new();
    for t in trades {
        by_tier.entry(t.entry_tier).or_default().push(t);
    }
    let mut t_tier = Table::new();
    t_tier.load_preset(UTF8_FULL);
    t_tier.set_header(vec![
        Cell::new("Tier"),
        Cell::new("Trades"),
        Cell::new("Win%"),
        Cell::new("Gross P&L"),
        Cell::new("Avg/Trade"),
        Cell::new("Net P&L"),
        Cell::new("Net Win%"),
    ]);
    let tier_order = [Tier::Strong, Tier::Medium, Tier::Early, Tier::Weak];
    for tier in tier_order {
        if let Some(ts) = by_tier.get(&tier) {
            let n = ts.len();
            let wins = ts.iter().filter(|t| t.pnl > 0.0).count();
            let win_pct = wins as f64 / n as f64 * 100.0;
            let total: f64 = ts.iter().map(|t| t.pnl).sum();
            let avg = total / n as f64;
            let color = if total >= 0.0 { Color::Green } else { Color::Red };
            let net_total: f64 = ts.iter().map(|t| t.net_pnl).sum();
            let net_wins = ts.iter().filter(|t| t.net_pnl > 0.0).count();
            let net_win_pct = net_wins as f64 / n as f64 * 100.0;
            let net_color = if net_total >= 0.0 { Color::Green } else { Color::Red };
            t_tier.add_row(vec![
                Cell::new(tier_label(tier)),
                Cell::new(n),
                Cell::new(format!("{:.0}%", win_pct)),
                Cell::new(format!("${:+.2}", total)).fg(color),
                Cell::new(format!("${:+.2}", avg)),
                Cell::new(format!("${:+.2}", net_total)).fg(net_color),
                Cell::new(format!("{:.0}%", net_win_pct)),
            ]);
        }
    }
    println!("{t_tier}");

    // === Exit reason breakdown ===
    println!();
    let mut by_reason: HashMap<&'static str, (usize, f64)> = HashMap::new();
    for t in trades {
        let e = by_reason.entry(t.exit_reason).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += t.pnl;
    }
    println!("Exit reasons: {:?}", by_reason);

    // === Warnings ===
    println!();
    println!("═══════════════ WARNINGS ═══════════════");
    println!("- Backtest учитывает только funding P&L. Не моделируется:");
    println!("    spread, slippage, маркет-импакт, liquidation, gas/funding fees биржи.");
    println!("- Volume для scoring — текущий из funding_snapshots (исторических нет в API).");
    println!("- Tier классификация адаптирована под hourly cadence (24h окно вместо 1h@5min).");
    println!("- Все позиции считаются мгновенно открываемыми/закрываемыми по mark.");
    println!("- Изменение mark price (PnL от цены) не моделируется — только funding accruals.");
    println!("- Funding granularity = 1 час (Hyperliquid публикует hourly).");
}

// silence unused
#[allow(dead_code)]
fn _format_usd_hint(v: f64) -> String { format_usd(v) }
