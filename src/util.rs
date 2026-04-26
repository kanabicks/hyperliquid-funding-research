// Общие хелперы, которыми пользуются и main.rs (scan/persistent), и watch.rs.
//
// Вынес чтобы не дублировать. `pub` — потому что обращаемся из других модулей
// этого же binary crate (`crate::util::format_usd`).

// 1234567 -> "1.2MB" / "456.0KB" / "999B". Для лога db size и UI.
pub fn format_bytes(b: u64) -> String {
    let bf = b as f64;
    if bf >= 1_073_741_824.0 {
        format!("{:.1}GB", bf / 1_073_741_824.0)
    } else if bf >= 1_048_576.0 {
        format!("{:.1}MB", bf / 1_048_576.0)
    } else if bf >= 1024.0 {
        format!("{:.1}KB", bf / 1024.0)
    } else {
        format!("{}B", b)
    }
}

// $1,234,567 -> "$1.23M" / "$456.7k". Чтобы таблицы не разъезжались.
pub fn format_usd(v: f64) -> String {
    if v >= 1_000_000_000.0 {
        format!("${:.2}B", v / 1_000_000_000.0)
    } else if v >= 1_000_000.0 {
        format!("${:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("${:.1}k", v / 1_000.0)
    } else {
        format!("${:.0}", v)
    }
}
