// === Imports ===
// anyhow::Result — удобный wrapper над Result<T, Box<dyn Error>>.
// Позволяет писать `?` на любых ошибках без объявления своих enum'ов.
use anyhow::{Context, Result};

// serde::Deserialize — derive macro, которая генерирует код десериализации из JSON.
// Поля struct'а с этим трейтом маппятся на JSON-поля по имени (или через #[serde(rename)]).
use serde::Deserialize;

// colored — расширение для String/&str: ".red()", ".green()" и т.д.
use colored::Colorize;

// comfy-table — рисует ASCII-таблицы в терминале с выравниванием/цветами.
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL};

// clap derive — парсинг CLI аргументов через #[derive(Parser)] на структуре.
// Под капотом генерирует код, который читает std::env::args(), валидирует
// и собирает в нашу Cli-struct.
use clap::{Parser, Subcommand};

// chrono для форматирования времени:
// - Local + TimeZone — "Last seen" в persistent выводе (локальное время для человека)
// - Utc + SecondsFormat — RFC3339 для production-логов collect mode
use chrono::{Local, SecondsFormat, TimeZone, Utc};

// sqlx::Row нужен чтобы вытаскивать значения из row.try_get::<T, _>("col").
// SqlitePool — пул соединений (внутри один-два, для sqlite этого достаточно).
// SqliteConnectOptions — конфиг подключения с .create_if_missing(true).
use sqlx::{Row as SqlxRow, SqlitePool, sqlite::{SqliteConnectOptions, SqlitePoolOptions}};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Внутренние модули. Объявляем чтобы файлы src/util.rs и src/watch.rs
// были подцеплены в этот binary crate. Доступ — через `crate::util::...`.
mod alerts;
mod backtest;
mod util;
mod watch;

// Хелперы, которыми пользуются и scan/persistent (тут), и watch.rs.
use crate::util::{format_bytes, format_usd};

// === CLI ===
//
// `cargo run -- scan` / `collect` / `persistent --hours 6 --min-apr 50`
// clap по дефолту юзает kebab-case для имён subcommand: Scan -> "scan".

#[derive(Parser)]
#[command(version, about = "Hyperliquid funding scanner")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Один разовый снапшот в терминал
    Scan,
    /// Бесконечный loop, пишем снапшот каждые --interval-seconds в SQLite
    Collect {
        /// Интервал между снапшотами в секундах (default 300 = 5 минут).
        /// Меньше для тестов, больше для экономии API.
        #[arg(long, default_value_t = 300)]
        interval_seconds: u64,
    },
    /// Показать перпы со стабильно высоким APR за последние N часов
    Persistent {
        #[arg(long, default_value_t = 6)]
        hours: u32,
        #[arg(long, default_value_t = 50.0)]
        min_apr: f64,
    },
    /// TUI live dashboard для мониторинга работы collector
    Watch,
    /// Periodic poller с Telegram-уведомлениями о tier-переходах
    AlertMonitor,
    /// Backtest стратегии на исторических funding rates
    Backtest {
        /// Сколько дней истории брать
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// Стартовый капитал в долларах
        #[arg(long, default_value_t = 1000.0)]
        capital: f64,
        /// Максимум одновременных позиций
        #[arg(long, default_value_t = 3)]
        max_positions: usize,
        /// Минимальный tier для входа: strong/medium/early
        #[arg(long, value_enum, default_value = "medium")]
        min_tier: backtest::CliTier,
        /// Сколько максимум держать позицию (часов)
        #[arg(long, default_value_t = 24)]
        hold_hours: u32,
        /// Bid-ask spread в bps (1 bp = 0.01%)
        #[arg(long, default_value_t = 20.0)]
        spread_bps: f64,
        /// Hyperliquid taker fee bps
        #[arg(long, default_value_t = 4.5)]
        taker_fee_bps: f64,
        /// Hyperliquid maker fee bps
        #[arg(long, default_value_t = 1.5)]
        maker_fee_bps: f64,
        /// Использовать limit (maker) order на entry вместо market
        #[arg(long, default_value_t = false)]
        use_maker: bool,
        /// Дополнительный slippage bps
        #[arg(long, default_value_t = 10.0)]
        slippage_bps: f64,
        /// Override tier system: входить если |funding_apr| >= threshold (стабильность std/|avg|<0.7).
        /// Sizing в этом режиме фиксированный 10% капитала.
        #[arg(long)]
        min_funding_apr: Option<f64>,
        /// Логика выхода: std (current), max-hold-only, hysteresis.
        #[arg(long, value_enum, default_value = "std")]
        exit_strategy: backtest::CliExitStrategy,
        /// Только для exit_strategy=hysteresis: entry threshold.
        #[arg(long, default_value_t = 200.0)]
        hysteresis_entry_apr: f64,
        /// Только для exit_strategy=hysteresis: exit threshold.
        #[arg(long, default_value_t = 50.0)]
        hysteresis_exit_apr: f64,
    },
    /// Sweep по 96 комбинациям параметров: maker × min_apr × hold × max_pos
    BacktestSweep {
        /// Сколько дней истории брать
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// Стартовый капитал в долларах
        #[arg(long, default_value_t = 1000.0)]
        capital: f64,
    },
}

// === Структуры под JSON-ответ Hyperliquid ===
//
// Hyperliquid endpoint /info с body {"type":"metaAndAssetCtxs"} возвращает
// массив из ДВУХ элементов: [Meta, Vec<AssetCtx>].
// universe[i] и ctxs[i] идут параллельно — индекс это и есть ID ассета.

// AssetMeta = метаданные одного перпа (имя, плечо, точность размера).
#[derive(Debug, Deserialize)]
struct AssetMeta {
    name: String,
    // serde по умолчанию ищет поле с тем же именем, что и в Rust-структуре.
    // В JSON оно называется "maxLeverage" (camelCase) — переименовываем.
    #[serde(rename = "maxLeverage")]
    max_leverage: u32,
    // szDecimals нам пока не нужен, но оставим для будущего.
    // serde::Deserialize по дефолту не падает если в JSON есть лишние поля,
    // но падает если в Rust-структуре есть поле, которого нет в JSON
    // (если только не #[serde(default)]).
    #[serde(rename = "szDecimals")]
    #[allow(dead_code)]
    sz_decimals: u32,
    // Делистнутые ассеты помечены `"isDelisted": true`. Поля может не быть,
    // поэтому Option + #[serde(default)] = None если ключа нет.
    #[serde(rename = "isDelisted", default)]
    is_delisted: Option<bool>,
}

// AssetCtx = живой контекст: цены, funding, объём, OI.
// ВНИМАНИЕ: Hyperliquid возвращает ВСЕ числа как строки ("0.0000072217"),
// поэтому здесь поля String, а в f64 парсим вручную дальше.
#[derive(Debug, Deserialize)]
struct AssetCtx {
    funding: String,
    #[serde(rename = "openInterest")]
    open_interest: String,
    #[serde(rename = "dayNtlVlm")]
    day_ntl_vlm: String,
    #[serde(rename = "markPx")]
    mark_px: String,
}

// MetaResponse = "обёртка" с полем universe внутри meta-объекта.
// Это первый элемент массива из ответа.
#[derive(Debug, Deserialize)]
struct MetaResponse {
    universe: Vec<AssetMeta>,
}

// Объединённая запись для отображения. Это уже наша внутренняя модель,
// после того как мы зипнули meta+ctx и распарсили строки в f64.
#[derive(Debug)]
struct ScanRow {
    name: String,
    max_leverage: u32,
    mark_px: f64,
    funding_hourly: f64,    // как пришло от API
    funding_apr: f64,       // funding_hourly * 24 * 365 * 100 (в процентах)
    open_interest_usd: f64, // OI в монетах * mark_px = USD-нотионал
    day_volume_usd: f64,    // dayNtlVlm уже в USD
}

// Результат persistent-запроса: одна строчка = один ассет с агрегатами.
#[derive(Debug)]
struct PersistentRow {
    asset: String,
    avg_apr: f64,
    std_apr: f64,
    last_apr: f64,
    snap_count: i64,
    last_ts: i64,
    day_volume_usd: f64,
    mark_price: f64,
    max_leverage: u32,
}

// === Точка входа ===
// #[tokio::main] разворачивается в обычный fn main(), который запускает
// tokio runtime и блокируется на нашем async-теле. "full" в фичах tokio
// даёт нам и многопоточный runtime, и timers, и net, и macros.
#[tokio::main]
async fn main() -> Result<()> {
    // Парсим CLI. .parse() сам прочитает std::env::args, и если что-то не так —
    // напечатает помощь и завершит процесс.
    let cli = Cli::parse();

    // HTTP-клиент. В reqwest клиент держит connection pool — создаём один раз.
    let client = reqwest::Client::new();

    // Диспатч по subcommand'у.
    match cli.command {
        Command::Scan => {
            let rows = fetch_snapshot(&client).await?;
            print_scan_table(&rows);
        }
        Command::Collect { interval_seconds } => {
            let pool = open_db().await?;
            collect_loop(&pool, &client, interval_seconds).await?;
        }
        Command::Persistent { hours, min_apr } => {
            let pool = open_db().await?;
            show_persistent(&pool, hours, min_apr).await?;
        }
        Command::Watch => {
            watch::run().await?;
        }
        Command::AlertMonitor => {
            alerts::run().await?;
        }
        Command::Backtest {
            days,
            capital,
            max_positions,
            min_tier,
            hold_hours,
            spread_bps,
            taker_fee_bps,
            maker_fee_bps,
            use_maker,
            slippage_bps,
            min_funding_apr,
            exit_strategy,
            hysteresis_entry_apr,
            hysteresis_exit_apr,
        } => {
            backtest::run(backtest::BacktestArgs {
                days,
                capital,
                max_positions,
                min_tier,
                hold_hours,
                spread_bps,
                taker_fee_bps,
                maker_fee_bps,
                use_maker,
                slippage_bps,
                min_funding_apr,
                exit_strategy,
                hysteresis_entry_apr,
                hysteresis_exit_apr,
            })
            .await?;
        }
        Command::BacktestSweep { days, capital } => {
            backtest::run_sweep(days, capital).await?;
        }
    }

    Ok(())
}

// === Снапшот через API ===
//
// Тянем JSON, парсим в (MetaResponse, Vec<AssetCtx>), зипаем по индексу,
// фильтруем делистнутые + лоулик, сортируем по |APR| убывающе.
// Возвращаем плоский Vec<ScanRow>.
async fn fetch_snapshot(client: &reqwest::Client) -> Result<Vec<ScanRow>> {
    let body = serde_json::json!({ "type": "metaAndAssetCtxs" });

    // Тип ответа — кортеж (MetaResponse, Vec<AssetCtx>). serde умеет
    // парсить JSON-массив фиксированной длины в Rust-кортеж: первый
    // элемент массива → .0, второй → .1.
    let (meta, ctxs): (MetaResponse, Vec<AssetCtx>) = client
        .post("https://api.hyperliquid.xyz/info")
        .json(&body)
        .send()
        .await
        .context("HTTP-запрос к Hyperliquid не прошёл")?
        .error_for_status()
        .context("Hyperliquid вернул не-2xx")?
        .json()
        .await
        .context("не получилось распарсить JSON-ответ")?;

    if meta.universe.len() != ctxs.len() {
        anyhow::bail!(
            "длина universe ({}) != длина ctxs ({})",
            meta.universe.len(),
            ctxs.len()
        );
    }

    let mut rows: Vec<ScanRow> = meta
        .universe
        .into_iter()
        .zip(ctxs.into_iter())
        .filter_map(|(m, c)| {
            if m.is_delisted.unwrap_or(false) {
                return None;
            }

            let funding_hourly = c.funding.parse::<f64>().ok()?;
            let open_interest = c.open_interest.parse::<f64>().ok()?;
            let day_ntl_vlm = c.day_ntl_vlm.parse::<f64>().ok()?;
            let mark_px = c.mark_px.parse::<f64>().ok()?;

            // Hyperliquid funding — ПОЧАСОВОЙ. Аннуализация: * 24 * 365.
            let funding_apr = funding_hourly * 24.0 * 365.0 * 100.0;
            let open_interest_usd = open_interest * mark_px;

            Some(ScanRow {
                name: m.name,
                max_leverage: m.max_leverage,
                mark_px,
                funding_hourly,
                funding_apr,
                open_interest_usd,
                day_volume_usd: day_ntl_vlm,
            })
        })
        .filter(|r| r.day_volume_usd > 100_000.0)
        .collect();

    rows.sort_by(|a, b| {
        b.funding_apr
            .abs()
            .partial_cmp(&a.funding_apr.abs())
            .unwrap()
    });

    Ok(rows)
}

// === Печать таблицы ===
fn print_scan_table(rows: &[ScanRow]) {
    let top: Vec<&ScanRow> = rows.iter().take(20).collect();

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec![
        Cell::new("#"),
        Cell::new("Symbol"),
        Cell::new("Mark $"),
        Cell::new("Funding/h %"),
        Cell::new("APR %"),
        Cell::new("OI $"),
        Cell::new("24h Vol $"),
        Cell::new("Lev"),
    ]);

    for (i, r) in top.iter().enumerate() {
        let apr_color = if r.funding_apr >= 0.0 {
            Color::Green
        } else {
            Color::Red
        };
        table.add_row(vec![
            Cell::new(i + 1),
            Cell::new(&r.name).fg(Color::Cyan),
            Cell::new(format!("{:.4}", r.mark_px)),
            Cell::new(format!("{:.5}", r.funding_hourly * 100.0)),
            Cell::new(format!("{:+.2}", r.funding_apr)).fg(apr_color),
            Cell::new(format_usd(r.open_interest_usd)),
            Cell::new(format_usd(r.day_volume_usd)),
            Cell::new(format!("{}x", r.max_leverage)),
        ]);
    }

    println!(
        "\n{} {} liquid perps (24h vol > $100k), top 20 by |APR|\n",
        "Hyperliquid funding scanner —".bold(),
        rows.len().to_string().yellow()
    );
    println!("{table}");
}

// === SQLite ===
//
// Открываем (создаём при необходимости) ./data/snapshots.db,
// проигрываем CREATE TABLE / INDEX IF NOT EXISTS — это и есть наша "миграция".

async fn open_db() -> Result<SqlitePool> {
    // Создаём директорию ./data, если её нет (sqlite не создаст её сам).
    std::fs::create_dir_all("./data").context("создаём ./data")?;

    // SqliteConnectOptions — явная альтернатива URL-строке "sqlite://...".
    // .create_if_missing(true) = автосоздание .db файла.
    let opts = SqliteConnectOptions::new()
        .filename("./data/snapshots.db")
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .context("подключение к SQLite не удалось")?;

    // Schema. IF NOT EXISTS — идемпотентно, можно гонять при каждом старте.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS funding_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            asset TEXT NOT NULL,
            funding_hourly REAL NOT NULL,
            funding_apr REAL NOT NULL,
            mark_price REAL NOT NULL,
            day_volume_usd REAL NOT NULL,
            open_interest_usd REAL NOT NULL,
            max_leverage INTEGER NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .context("CREATE TABLE funding_snapshots")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_asset_ts \
         ON funding_snapshots(asset, timestamp DESC)",
    )
    .execute(&pool)
    .await
    .context("CREATE INDEX idx_asset_ts")?;

    Ok(pool)
}

// === Collect loop ===
//
// Каждые `interval_seconds`: fetch -> insert all rows в одной транзакции -> log.
// Транзакция критична: 147 отдельных INSERT'ов с автокоммитом = 147 fsync'ов
// (секунды); в одной транзакции — миллисекунды.
//
// Graceful shutdown: tokio::select! гонит две ветки параллельно — рабочую
// и signal-handler. Что разрешится первым, отменяет вторую через future
// cancellation. SQLite tx, недокоммиченная при отмене, автоматически
// откатывается при drop'е соединения. Так мы не оставляем мусор в БД.
async fn collect_loop(
    pool: &SqlitePool,
    client: &reqwest::Client,
    interval_seconds: u64,
) -> Result<()> {
    let interval = Duration::from_secs(interval_seconds);

    println!(
        "Starting collect mode, interval: {}s, db: ./data/snapshots.db",
        interval_seconds
    );

    loop {
        let started = std::time::Instant::now();

        // Фаза 1: снапшот + запись. Прерываемая Ctrl+C.
        // `biased` — детерминированный порядок проверки веток в select!:
        // сначала смотрим signal, потом работу. Без него tokio выбирает случайно
        // и в редких случаях может проигнорить мгновенный сигнал.
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                println!("Shutdown signal received, exiting cleanly");
                pool.close().await;
                return Ok(());
            }
            result = do_one_snapshot(pool, client) => {
                if let Err(e) = result {
                    let now_str = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                    eprintln!("[{}] snapshot error: {:#}", now_str, e);
                }
            }
        }

        // Фаза 2: спим оставшееся до следующего тика. Тоже прерываемо.
        let elapsed = started.elapsed();
        if elapsed < interval {
            let remaining = interval - elapsed;
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    println!("Shutdown signal received, exiting cleanly");
                    pool.close().await;
                    return Ok(());
                }
                _ = tokio::time::sleep(remaining) => {}
            }
        }
    }
}

// Один цикл: fetch + insert + лог.
// Вынесено отдельной функцией чтобы целиком отменяться через select!.
async fn do_one_snapshot(pool: &SqlitePool, client: &reqwest::Client) -> Result<()> {
    let rows = fetch_snapshot(client).await?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let count = rows.len();

    let mut tx = pool.begin().await.context("BEGIN tx")?;
    for r in &rows {
        sqlx::query(
            "INSERT INTO funding_snapshots
                (timestamp, asset, funding_hourly, funding_apr, mark_price,
                 day_volume_usd, open_interest_usd, max_leverage)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(ts)
        .bind(&r.name)
        .bind(r.funding_hourly)
        .bind(r.funding_apr)
        .bind(r.mark_px)
        .bind(r.day_volume_usd)
        .bind(r.open_interest_usd)
        .bind(r.max_leverage as i64)
        .execute(&mut *tx)
        .await
        .context("INSERT funding_snapshots")?;
    }
    tx.commit().await.context("COMMIT tx")?;

    // Размер файла + общий счётчик строк — лёгкая инструментация.
    let db_size = std::fs::metadata("./data/snapshots.db")
        .map(|m| m.len())
        .unwrap_or(0);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM funding_snapshots")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    let now_str = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    println!(
        "[{}] Saved {} snapshots, db size: {}, total snapshots: {}",
        now_str,
        count,
        format_bytes(db_size),
        total
    );
    Ok(())
}

// === Persistent query ===
//
// За последние `hours` часов агрегируем по ассету: avg(APR), avg(APR^2)
// (для дисперсии), count, max(timestamp). Std считаем в Rust:
// std = sqrt(avg(x^2) - avg(x)^2). Дальше ещё один JOIN на funding_snapshots
// чтобы вытащить значения из самого свежего снапшота (volume, mark, leverage).
async fn show_persistent(pool: &SqlitePool, hours: u32, min_apr: f64) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let cutoff = now - (hours as i64) * 3600;

    // Ожидаемое число снапшотов = 12 в час (1 раз в 5 мин). Требуем минимум 60%.
    let expected = (hours as i64) * 12;
    let min_count = ((expected as f64) * 0.6).ceil() as i64;

    // CTE-запрос:
    //  agg       — агрегаты по ассету за окно
    //  last_snap — присоединяем "самый свежий снапшот" каждого ассета,
    //              чтобы вытащить актуальные volume/mark/leverage.
    let sql = r#"
        WITH agg AS (
            SELECT
                asset,
                AVG(funding_apr) AS avg_apr,
                AVG(funding_apr * funding_apr) AS avg_apr_sq,
                COUNT(*) AS snap_count,
                MAX(timestamp) AS last_ts
            FROM funding_snapshots
            WHERE timestamp >= ?
            GROUP BY asset
        ),
        last_snap AS (
            SELECT s.asset,
                   s.day_volume_usd,
                   s.mark_price,
                   s.max_leverage,
                   s.funding_apr AS last_apr
            FROM funding_snapshots s
            INNER JOIN agg ON agg.asset = s.asset AND agg.last_ts = s.timestamp
        )
        SELECT
            a.asset,
            a.avg_apr,
            a.avg_apr_sq,
            a.snap_count,
            a.last_ts,
            l.day_volume_usd,
            l.mark_price,
            l.max_leverage,
            l.last_apr
        FROM agg a
        JOIN last_snap l ON a.asset = l.asset
        ORDER BY ABS(a.avg_apr) DESC
    "#;

    let raw_rows = sqlx::query(sql)
        .bind(cutoff)
        .fetch_all(pool)
        .await
        .context("persistent query")?;

    // Считаем std_apr в Rust и применяем фильтры.
    let mut filtered: Vec<PersistentRow> = Vec::new();
    for row in raw_rows {
        let asset: String = row.try_get("asset")?;
        let avg_apr: f64 = row.try_get("avg_apr")?;
        let avg_apr_sq: f64 = row.try_get("avg_apr_sq")?;
        let snap_count: i64 = row.try_get("snap_count")?;
        let last_ts: i64 = row.try_get("last_ts")?;
        let day_volume_usd: f64 = row.try_get("day_volume_usd")?;
        let mark_price: f64 = row.try_get("mark_price")?;
        let max_leverage: i64 = row.try_get("max_leverage")?;
        let last_apr: f64 = row.try_get("last_apr")?;

        // Population stddev: var = E[X^2] - E[X]^2.
        // .max(0.0) на случай флоат-погрешности (могло выйти -1e-15).
        let variance = (avg_apr_sq - avg_apr * avg_apr).max(0.0);
        let std_apr = variance.sqrt();

        if snap_count < min_count {
            continue;
        }
        if avg_apr.abs() < min_apr {
            continue;
        }
        // Стабильность: разброс < 30% от среднего.
        // Если avg_apr близок к нулю — отношение взлетает, но мы уже отфильтровали по min_apr.
        if std_apr >= avg_apr.abs() * 0.3 {
            continue;
        }
        if day_volume_usd < 1_000_000.0 {
            continue;
        }

        filtered.push(PersistentRow {
            asset,
            avg_apr,
            std_apr,
            last_apr,
            snap_count,
            last_ts,
            day_volume_usd,
            mark_price,
            max_leverage: max_leverage as u32,
        });
    }

    print_persistent_table(&filtered, hours, min_apr, min_count);
    Ok(())
}

fn print_persistent_table(rows: &[PersistentRow], hours: u32, min_apr: f64, min_count: i64) {
    println!(
        "\n{} window={}h, min |avg APR|={:.0}%, min snapshots={}, std/|avg| < 0.30, last vol > $1M",
        "Persistent funding —".bold(),
        hours,
        min_apr,
        min_count,
    );

    if rows.is_empty() {
        println!(
            "\n{} нет ассетов, удовлетворяющих фильтрам. Возможно мало данных в БД — проверь `cargo run -- collect`.\n",
            "(empty)".dimmed()
        );
        return;
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec![
        Cell::new("#"),
        Cell::new("Symbol"),
        Cell::new("Avg APR %"),
        Cell::new("Std APR %"),
        Cell::new("Last APR %"),
        Cell::new("Snaps"),
        Cell::new("Last seen"),
        Cell::new("Mark $"),
        Cell::new("24h Vol $"),
        Cell::new("Lev"),
    ]);

    for (i, r) in rows.iter().enumerate() {
        let apr_color = if r.avg_apr >= 0.0 {
            Color::Green
        } else {
            Color::Red
        };
        // Local.timestamp_opt(secs, nsecs) — может вернуть None для нелегитимных
        // значений. У нас секунды из SystemTime — всегда валидны.
        let last_seen = Local
            .timestamp_opt(r.last_ts, 0)
            .single()
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "?".to_string());

        table.add_row(vec![
            Cell::new(i + 1),
            Cell::new(&r.asset).fg(Color::Cyan),
            Cell::new(format!("{:+.2}", r.avg_apr)).fg(apr_color),
            Cell::new(format!("{:.2}", r.std_apr)),
            Cell::new(format!("{:+.2}", r.last_apr)),
            Cell::new(r.snap_count),
            Cell::new(last_seen),
            Cell::new(format!("{:.4}", r.mark_price)),
            Cell::new(format_usd(r.day_volume_usd)),
            Cell::new(format!("{}x", r.max_leverage)),
        ]);
    }

    println!("{table}");
}

// format_usd / format_bytes теперь в src/util.rs.
