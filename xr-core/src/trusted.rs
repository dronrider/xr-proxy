//! Trusted-network (SSID) matching for the auto-pause feature (task 3b-2).
//!
//! When the phone joins a Wi-Fi the user marked as "trusted" (typically the
//! home network already behind an xr-client router), the app pauses its own
//! tunnel so traffic isn't double-tunnelled. The decision boils down to one
//! string comparison: does the current SSID match any trusted entry?
//!
//! The matching lives here — in Rust — rather than in Kotlin so the fiddly
//! normalization (Android wraps SSIDs in quotes, hidden/permission-denied
//! networks come back as sentinels) is covered by unit tests. Android has no
//! automated test layer in this project, so business logic gets pulled down
//! into the core engine where it can be cried.

/// Normalize a raw SSID as reported by Android's `WifiInfo.getSSID()`.
///
/// Android returns the SSID wrapped in double quotes for valid UTF-8
/// (`"MyWifi"`), or an unquoted hex string for non-UTF-8 names. When the SSID
/// is unavailable — hidden network, or the app lacks location permission /
/// location services are off — it returns the literal `<unknown ssid>` (older
/// builds) or `0x` / empty. All of those map to `None` (i.e. "no usable SSID,
/// don't match anything").
pub fn normalize_ssid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Strip the surrounding quotes Android adds for UTF-8 SSIDs, but only when
    // BOTH are present — a lone quote is part of the name.
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim();

    if unquoted.is_empty() {
        return None;
    }
    // Sentinels for "SSID not available to this caller".
    if unquoted.eq_ignore_ascii_case("<unknown ssid>") || unquoted == "0x" {
        return None;
    }
    Some(unquoted.to_string())
}

/// True if `raw_current` (a raw `WifiInfo.getSSID()` value) matches any entry
/// in the trusted list. Comparison is case-insensitive and ignores the quote
/// wrapping / surrounding whitespace on both sides. An unusable current SSID
/// (see [`normalize_ssid`]) never matches.
pub fn ssid_matches(raw_current: &str, trusted: &[String]) -> bool {
    let current = match normalize_ssid(raw_current) {
        Some(c) => c,
        None => return false,
    };
    trusted.iter().any(|entry| {
        normalize_ssid(entry).is_some_and(|t| t.eq_ignore_ascii_case(&current))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_surrounding_quotes() {
        assert_eq!(normalize_ssid("\"HomeWifi\""), Some("HomeWifi".to_string()));
    }

    #[test]
    fn normalize_keeps_unquoted_name() {
        assert_eq!(normalize_ssid("HomeWifi"), Some("HomeWifi".to_string()));
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_ssid("  HomeWifi  "), Some("HomeWifi".to_string()));
        assert_eq!(normalize_ssid("\" Home \""), Some("Home".to_string()));
    }

    #[test]
    fn normalize_rejects_unknown_sentinels() {
        assert_eq!(normalize_ssid("<unknown ssid>"), None);
        assert_eq!(normalize_ssid("\"<unknown ssid>\""), None);
        assert_eq!(normalize_ssid("<UNKNOWN SSID>"), None);
        assert_eq!(normalize_ssid("0x"), None);
        assert_eq!(normalize_ssid(""), None);
        assert_eq!(normalize_ssid("   "), None);
        assert_eq!(normalize_ssid("\"\""), None);
    }

    #[test]
    fn normalize_keeps_lone_quote_in_name() {
        // A single leading quote is not the Android wrapper — keep it.
        assert_eq!(normalize_ssid("\"odd"), Some("\"odd".to_string()));
    }

    #[test]
    fn matches_exact_and_quoted() {
        let trusted = vec!["HomeWifi".to_string()];
        assert!(ssid_matches("\"HomeWifi\"", &trusted));
        assert!(ssid_matches("HomeWifi", &trusted));
    }

    #[test]
    fn matches_case_insensitively() {
        let trusted = vec!["HomeWifi".to_string()];
        assert!(ssid_matches("\"homewifi\"", &trusted));
        assert!(ssid_matches("\"HOMEWIFI\"", &trusted));
    }

    #[test]
    fn matches_when_trusted_entry_is_quoted_too() {
        let trusted = vec!["\"HomeWifi\"".to_string(), " Office ".to_string()];
        assert!(ssid_matches("\"HomeWifi\"", &trusted));
        assert!(ssid_matches("Office", &trusted));
    }

    #[test]
    fn no_match_for_other_network() {
        let trusted = vec!["HomeWifi".to_string()];
        assert!(!ssid_matches("\"Starbucks\"", &trusted));
    }

    #[test]
    fn no_match_for_unknown_or_empty_current() {
        let trusted = vec!["HomeWifi".to_string()];
        assert!(!ssid_matches("<unknown ssid>", &trusted));
        assert!(!ssid_matches("", &trusted));
    }

    #[test]
    fn no_match_against_empty_trusted_list() {
        assert!(!ssid_matches("\"HomeWifi\"", &[]));
    }

    #[test]
    fn empty_trusted_entries_are_ignored() {
        let trusted = vec!["".to_string(), "<unknown ssid>".to_string()];
        assert!(!ssid_matches("\"HomeWifi\"", &trusted));
    }
}
