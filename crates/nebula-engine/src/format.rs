//! 结果集渲染辅助:时间戳格式化、关键词/关键点/标签的展示格式。
//! Unix 毫秒 → UTC "YYYY-MM-DD HH:MM:SS"(不引入 chrono,算法见下文)。

use nebula_core::{Keyword, MAX_KEY_POINTS, MAX_KEYWORDS};

/// Unix 毫秒时间戳格式化为 UTC 日期时间。
pub fn fmt_time(millis: nebula_core::Timestamp) -> String {
    if millis < 0 {
        return "-".into();
    }
    let secs = millis / 1000;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
}

/// Howard Hinnant 的 days_from_civil 逆运算:Unix 天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m as u32, d as u32)
}

/// `rust(0.91), 内存(0.45)` 形式。
pub fn fmt_keywords(kws: &[Keyword]) -> String {
    kws.iter()
        .take(MAX_KEYWORDS)
        .map(|k| format!("{}({:.2})", k.term, k.weight))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `rust, 内存, 所有权` 纯词项形式。
pub fn keywords_inline(kws: &[Keyword]) -> String {
    kws.iter()
        .take(MAX_KEYWORDS)
        .map(|k| k.term.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// 关键点多行展示(SELECT * 时用)。
pub fn fmt_key_points(points: &[String]) -> String {
    points
        .iter()
        .take(MAX_KEY_POINTS)
        .map(|p| format!("• {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 标签逗号拼接。
pub fn fmt_tags(tags: &[String]) -> String {
    tags.join(", ")
}

/// `0.75` 两位小数。
pub fn fmt_importance(v: f32) -> String {
    format!("{v:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::Keyword;

    #[test]
    fn time_format_known_values() {
        // 1970-01-01 00:00:00 UTC = 0
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00");
        // 2000-01-01 00:00:00 UTC = 946684800 s
        assert_eq!(fmt_time(946_684_800_000), "2000-01-01 00:00:00");
        // 2024-02-29 00:00:00 UTC = 1709164800 s
        assert_eq!(fmt_time(1_709_164_800_000), "2024-02-29 00:00:00");
        // 2038-01-19 03:14:07 UTC(unix32 位上限附近)
        assert_eq!(fmt_time(2_147_483_647_000), "2038-01-19 03:14:07");
        assert_eq!(fmt_time(-5), "-");
    }

    #[test]
    fn keyword_and_point_formatting() {
        let kws = vec![Keyword::new("rust", 0.9123), Keyword::new("内存", 0.4)];
        assert_eq!(fmt_keywords(&kws), "rust(0.91), 内存(0.40)");
        assert_eq!(keywords_inline(&kws), "rust, 内存");
        assert_eq!(fmt_key_points(&["一条".into(), "二条".into()]), "• 一条\n• 二条");
        assert_eq!(fmt_tags(&["a".into(), "b".into()]), "a, b");
        assert_eq!(fmt_importance(0.5), "0.50");
        assert_eq!(fmt_time(-5), "-");
    }
}
