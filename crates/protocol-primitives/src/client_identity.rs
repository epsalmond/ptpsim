//! Client identity normalization — turn a raw host device name into the single
//! short name the camera accepts on BOTH the BLE `deviceNameString` and the
//! PTP/IP friendly name.
//!
//! The camera accepts any short name but silently drops an `InitCommandRequest`
//! whose two channels disagree (#109), so the host normalizes once and writes the
//! same value to both. The manifest single-sources them from one `terminalName`
//! runtime slot, so consistency is by construction — this just produces that
//! canonical value. Replaces client application's `cameraSafeDeviceName` +
//! `sharedRegistrationAndPTPIPName`.

/// Character cap shared by BLE registration and PTP/IP init. BLE's established
/// 18-character budget is narrower than reference app's 26 UTF-16-unit text budget, so
/// both channels use the BLE limit and remain byte-identical.
const MAX_NAME_CHARS: usize = 18;

/// Fallback when the input has no usable characters.
const FALLBACK: &str = "client application";

/// Normalize `raw` to a camera-safe client name: lowercase ASCII-alphanumeric
/// runs joined by a single `-`, capped at [`MAX_NAME_CHARS`], with surrounding
/// `-` trimmed, falling back to [`FALLBACK`] when nothing usable remains. Any
/// non-ASCII-alphanumeric character is a separator (the camera does not validate
/// name content, only that the two channels match).
pub fn normalize_client_name(raw: &str) -> String {
    let mut joined = String::new();
    let mut pending_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !joined.is_empty() {
                joined.push('-');
            }
            joined.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    let capped: String = joined.chars().take(MAX_NAME_CHARS).collect();
    let trimmed = capped.trim_matches('-');
    if trimmed.is_empty() {
        FALLBACK.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_joins_alphanumeric_runs() {
        assert_eq!(
            normalize_client_name(" Eric's iPad Pro "),
            "eric-s-ipad-pro"
        );
        assert_eq!(normalize_client_name("Pixel 6"), "pixel-6");
        assert_eq!(normalize_client_name("iPhone15,2"), "iphone15-2");
    }

    #[test]
    fn caps_at_the_field_size_and_trims_dashes() {
        assert_eq!(
            normalize_client_name("abcdefghijk-lmnop"),
            "abcdefghijk-lmnop"
        );
        assert_eq!(
            normalize_client_name("verylongdevicename-extra"),
            "verylongdevicename"
        );
        // The cap must never leave a trailing dash (would desync BLE vs PTP/IP).
        assert!(!normalize_client_name("abcdefghijklmno pqr").ends_with('-'));
    }

    #[test]
    fn empty_or_symbol_only_falls_back() {
        assert_eq!(normalize_client_name(""), "client application");
        assert_eq!(normalize_client_name("   "), "client application");
        assert_eq!(normalize_client_name("!!! ---"), "client application");
    }

    #[test]
    fn result_fits_the_ptpip_name_field() {
        for raw in [
            " Eric's iPad Pro ",
            "Pixel 6",
            "!!!",
            "长长长",
            "abcdefghijklmnop",
        ] {
            let n = normalize_client_name(raw);
            assert!(n.chars().count() <= MAX_NAME_CHARS);
            assert!(!n.starts_with('-') && !n.ends_with('-'));
            assert!(!n.is_empty());
        }
    }
}
