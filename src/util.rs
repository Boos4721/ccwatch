//! 通用工具:时间戳、时长格式化。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 时间戳(秒)。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把 Unix 秒换算成"天编号"(UTC 自然日),用于今日统计的跨日重置。
pub fn day_of(ts: u64) -> u64 {
    ts / 86_400
}

/// 把秒数格式化成人类可读时长:`1h 2m`、`42m`、`30s`。
pub fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m > 0 {
            format!("{}h {}m", h, m)
        } else {
            format!("{}h", h)
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_dur_buckets() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(60), "1m");
        assert_eq!(fmt_dur(150), "2m");
        assert_eq!(fmt_dur(3600), "1h");
        assert_eq!(fmt_dur(3720), "1h 2m");
    }

    #[test]
    fn day_of_resets() {
        assert_eq!(day_of(0), 0);
        assert_eq!(day_of(86_399), 0);
        assert_eq!(day_of(86_400), 1);
    }
}
