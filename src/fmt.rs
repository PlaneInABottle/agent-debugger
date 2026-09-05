//! Output-shrinking rules (browser-snapshot thinking, debugger domain).
//!
//! The Java bridge currently enforces these caps itself; this module will
//! own them crate-side when Python/Node adapters arrive (shared policy).

// shared policy for later phases
#![allow(dead_code)]

/// Default cap for a single inlined string value (chars, not bytes).
pub const MAX_STRING_LEN: usize = 200;
/// Default number of collection items inlined before `... N more`.
pub const MAX_ARRAY_ITEMS: usize = 3;

/// Truncate `s` to at most `limit` chars (Unicode-safe).
///
/// Returns the string unchanged when it fits. Otherwise returns the first
/// `limit` chars plus `… (+N more chars)`.
pub fn truncate_str(s: &str, limit: usize) -> String {
    let total = s.chars().count();
    if total <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit).collect();
    format!("{head}… (+{} more chars)", total - limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_unchanged() {
        assert_eq!(truncate_str("hello", 200), "hello");
    }

    #[test]
    fn exact_limit_unchanged() {
        let s = "a".repeat(10);
        assert_eq!(truncate_str(&s, 10), s);
    }

    #[test]
    fn long_string_truncated_with_count() {
        let s = "a".repeat(10);
        assert_eq!(truncate_str(&s, 3), "aaa… (+7 more chars)");
    }

    #[test]
    fn multibyte_boundary_safe() {
        // Emoji are multi-byte; truncation must not split them.
        let s = "😀".repeat(5);
        let out = truncate_str(&s, 3);
        assert_eq!(out, "😀😀😀… (+2 more chars)");
    }
}
