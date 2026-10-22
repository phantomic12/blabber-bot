//! Small formatting helpers shared by commands and announcements.

use std::time::Duration;

/// Formats a number of seconds as `m:ss` / `h:mm:ss`, or `LIVE` when unknown.
pub fn fmt_secs(secs: Option<f64>) -> String {
    match secs {
        Some(s) if s.is_finite() && s >= 0.0 => fmt_count(s.trunc() as u64),
        _ => "LIVE".to_string(),
    }
}

/// Formats a whole number of seconds as `m:ss` / `h:mm:ss`.
pub fn fmt_count(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Parses `90`, `1:30` or `1:02:03` into seconds.
pub fn parse_timestamp(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let parts: Vec<&str> = input.split(':').collect();

    let secs = match parts.as_slice() {
        [s] => s.parse::<u64>().map_err(|_| format!("'{input}' is not a valid timestamp"))?,
        [m, s] => {
            let m: u64 = m.parse().map_err(|_| format!("'{input}' is not a valid timestamp"))?;
            let s: u64 = s.parse().map_err(|_| format!("'{input}' is not a valid timestamp"))?;
            if s >= 60 {
                return Err(format!("'{input}' has seconds out of range"));
            }
            m * 60 + s
        },
        [h, m, s] => {
            let h: u64 = h.parse().map_err(|_| format!("'{input}' is not a valid timestamp"))?;
            let m: u64 = m.parse().map_err(|_| format!("'{input}' is not a valid timestamp"))?;
            let s: u64 = s.parse().map_err(|_| format!("'{input}' is not a valid timestamp"))?;
            if m >= 60 || s >= 60 {
                return Err(format!("'{input}' has minutes/seconds out of range"));
            }
            h * 3600 + m * 60 + s
        },
        _ => return Err(format!("'{input}' is not a valid timestamp")),
    };

    Ok(secs)
}

/// Renders a `▰▰▰▱▱▱▱` progress bar for a position within an optional total.
pub fn progress_bar(position: Duration, total: Option<f64>) -> String {
    const WIDTH: usize = 14;

    let Some(total) = total.filter(|t| t.is_finite() && *t > 0.0) else {
        return String::new();
    };

    let pos = position.as_secs_f64().clamp(0.0, total);
    let filled = ((pos / total) * WIDTH as f64).round() as usize;
    let filled = filled.min(WIDTH);

    format!(
        "`{}{}` **{}** / {}",
        "▰".repeat(filled),
        "▱".repeat(WIDTH - filled),
        fmt_count(pos as u64),
        fmt_count(total as u64),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("90"), Ok(90));
        assert_eq!(parse_timestamp("1:30"), Ok(90));
        assert_eq!(parse_timestamp("1:02:03"), Ok(3723));
        assert!(parse_timestamp("1:99").is_err());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_secs(Some(59.0)), "0:59");
        assert_eq!(fmt_secs(Some(3723.0)), "1:02:03");
        assert_eq!(fmt_secs(None), "LIVE");
    }
}
