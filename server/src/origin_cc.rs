use crate::browser::current_proxy_port;
use crate::config::Config;
use std::time::Duration;

pub struct CcDefaults {
    pub default_max_age: i32,
    pub max_max_age: i32,
}

pub struct OriginDirectives {
    pub max_age: i32,
}

/// Parse origin cache directives into a single wire `max_age` value.
///
/// Precedence order:
/// 1. `Cache-Control` directives (`no-store`/`private` → -1; `no-cache` → 0;
///    `s-maxage=N` > `max-age=N` → clamped positive).
/// 2. `Expires` minus `Date` if both parse as HTTP-dates.
/// 3. `defaults.default_max_age`.
///
/// The `max-age`/`s-maxage`/expires-derived values are clamped to
/// `defaults.max_max_age`.
pub fn parse_cache_control(
    cc: Option<&str>,
    expires: Option<&str>,
    date: Option<&str>,
    defaults: &CcDefaults,
) -> i32 {
    if let Some(cc) = cc {
        let lower = cc.to_ascii_lowercase();
        let tokens: Vec<&str> = lower.split(',').map(|t| t.trim()).collect();
        if tokens.iter().any(|t| *t == "no-store" || *t == "private") {
            return -1;
        }
        if tokens.iter().any(|t| *t == "no-cache") {
            return 0;
        }
        // Find s-maxage first (shared-cache overrides max-age for us).
        let s_maxage = tokens
            .iter()
            .find_map(|t| t.strip_prefix("s-maxage="))
            .and_then(|v| v.parse::<i64>().ok());
        let max_age = tokens
            .iter()
            .find_map(|t| t.strip_prefix("max-age="))
            .and_then(|v| v.parse::<i64>().ok());
        let picked = s_maxage.or(max_age);
        if let Some(secs) = picked {
            let clamped = secs.clamp(0, defaults.max_max_age as i64);
            return clamped as i32;
        }
        // Cache-Control present but no useful directive → default.
        return defaults.default_max_age;
    }
    if let (Some(exp), Some(dt)) = (expires, date) {
        if let (Ok(exp_ts), Ok(dt_ts)) = (
            httpdate::parse_http_date(exp),
            httpdate::parse_http_date(dt),
        ) {
            if let Ok(delta) = exp_ts.duration_since(dt_ts) {
                let secs = delta.as_secs().min(defaults.max_max_age as u64) as i32;
                return secs;
            }
        }
    }
    defaults.default_max_age
}

pub fn probe_origin_cc(url: &str, config: &Config) -> OriginDirectives {
    let defaults = CcDefaults {
        default_max_age: config.default_max_age_seconds,
        max_max_age: config.max_max_age_seconds,
    };
    let default_out = OriginDirectives { max_age: defaults.default_max_age };

    let proxy_port = match current_proxy_port() {
        Some(p) => p,
        None => return default_out,
    };
    let proxy = match reqwest::Proxy::all(format!("http://127.0.0.1:{}", proxy_port)) {
        Ok(p) => p,
        Err(_) => return default_out,
    };
    let client = match reqwest::blocking::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_millis(config.origin_cc_head_timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(_) => return default_out,
    };

    let headers = match client.head(url).send() {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => r.headers().clone(),
        Ok(r) if r.status().as_u16() == 405 => {
            // Retry as GET, discard body; several origins reject HEAD.
            match client.get(url).send() {
                Ok(r) if r.status().is_success() || r.status().is_redirection() => r.headers().clone(),
                _ => return default_out,
            }
        }
        Ok(_) => return default_out,
        Err(_) => return default_out,
    };

    let cc = headers.get("cache-control").and_then(|v| v.to_str().ok());
    let expires = headers.get("expires").and_then(|v| v.to_str().ok());
    let date = headers.get("date").and_then(|v| v.to_str().ok());
    OriginDirectives {
        max_age: parse_cache_control(cc, expires, date, &defaults),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> CcDefaults {
        CcDefaults { default_max_age: 3600, max_max_age: 2_592_000 }
    }

    #[test]
    fn cc_no_store_gives_negative() {
        assert_eq!(parse_cache_control(Some("no-store"), None, None, &defs()), -1);
    }

    #[test]
    fn cc_private_gives_negative() {
        assert_eq!(parse_cache_control(Some("private, max-age=600"), None, None, &defs()), -1);
    }

    #[test]
    fn cc_no_cache_gives_zero() {
        assert_eq!(parse_cache_control(Some("no-cache"), None, None, &defs()), 0);
    }

    #[test]
    fn cc_max_age_forwarded() {
        assert_eq!(parse_cache_control(Some("max-age=600"), None, None, &defs()), 600);
    }

    #[test]
    fn cc_s_maxage_takes_precedence_over_max_age() {
        assert_eq!(
            parse_cache_control(Some("s-maxage=60, max-age=600"), None, None, &defs()),
            60
        );
    }

    #[test]
    fn cc_max_age_clamped_to_cap() {
        assert_eq!(
            parse_cache_control(Some("max-age=99999999"), None, None, &defs()),
            2_592_000
        );
    }

    #[test]
    fn cc_expires_falls_back_when_no_cc() {
        // Expires = Date + 60s → 60.
        let out = parse_cache_control(
            None,
            Some("Fri, 06 Nov 2026 08:50:00 GMT"),
            Some("Fri, 06 Nov 2026 08:49:00 GMT"),
            &defs(),
        );
        assert_eq!(out, 60);
    }

    #[test]
    fn cc_missing_everything_gives_default() {
        assert_eq!(parse_cache_control(None, None, None, &defs()), 3600);
    }

    #[test]
    fn cc_malformed_gives_default() {
        assert_eq!(parse_cache_control(Some("bogus!!"), None, None, &defs()), 3600);
    }

    #[test]
    fn cc_expires_in_the_past_gives_default() {
        let out = parse_cache_control(
            None,
            Some("Fri, 06 Nov 2026 08:49:00 GMT"),
            Some("Fri, 06 Nov 2026 08:50:00 GMT"),
            &defs(),
        );
        assert_eq!(out, 3600);
    }
}
