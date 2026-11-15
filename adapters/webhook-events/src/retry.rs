use std::time::SystemTime;

pub(crate) fn retry_seconds(attempt: i64, retry_after: Option<&str>) -> i64 {
    // Exponential application backoff stops growing at five minutes. A longer
    // explicit receiver cooldown takes precedence. Clamp only to a DB-safe horizon.
    let exponent = (attempt - 1).clamp(0, 9) as u32;
    let backoff = (1i64 << exponent).min(300);
    let hint = retry_after
        .and_then(|value| {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                return Some(value.parse::<u64>().unwrap_or(u64::MAX));
            }
            let deadline = httpdate::parse_http_date(value).ok()?;
            let delay = deadline
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            Some(
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() != 0)),
            )
        })
        .unwrap_or(0);
    backoff.max(hint.min(365 * 24 * 60 * 60) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_caps_growth_and_honors_receiver_cooldown() {
        assert_eq!(retry_seconds(1, None), 1);
        assert_eq!(retry_seconds(6, None), 32);
        assert_eq!(retry_seconds(1000, None), 300);
        assert_eq!(retry_seconds(1000, Some("600")), 600);
        assert_eq!(retry_seconds(1, Some("invalid")), 1);
    }
}
