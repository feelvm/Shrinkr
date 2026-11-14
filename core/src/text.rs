//! Small string helpers for log- and error-facing text.
//!
//! Diagnostics embed user-controlled data (ffmpeg prints input paths on
//! almost every failure), so any byte-index slicing of them must respect
//! UTF-8 char boundaries: `&s[..300]` panics when byte 300 falls inside a
//! multi-byte character, and a panic inside an encoder worker both loses
//! the real error and leaks the temp output.

/// Truncate to at most `max` bytes, cutting back to the last UTF-8 char
/// boundary (never splitting a character, never panicking).
pub fn truncate_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Keep only the last `max` bytes, moving the start forward to the first
/// UTF-8 char boundary. Used for stderr tails, where the newest lines are
/// the relevant ones.
pub fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_never_splits_a_char() {
        // 'abécdé' is 8 bytes ('é' is 2); byte 6 is the second 'é' lead.
        let s = "abécdé";
        assert_eq!(truncate_head(s, 5), "abéc");
        assert_eq!(truncate_head(s, 6), "abécd");
        assert_eq!(truncate_head(s, 100), s);
        // Pure multibyte CJK (3 bytes/char): byte 2 is a continuation byte.
        assert_eq!(truncate_head("日本語", 2), "");
        assert_eq!(truncate_head("日本語", 3), "日");
    }

    #[test]
    fn tail_never_splits_a_char() {
        // Keeping the last 5/6 bytes of an 8-byte string walks the start
        // forward to the first char boundary.
        let s = "abécdé";
        assert_eq!(truncate_tail(s, 5), "cdé");
        assert_eq!(truncate_tail(s, 6), "écdé");
        assert_eq!(truncate_tail(s, 100), s);
        // 3-byte CJK: a 2-byte budget cannot hold a whole character, so the
        // result is empty; 4 bytes fits exactly one.
        assert_eq!(truncate_tail("日本語", 2), "");
        assert_eq!(truncate_tail("日本語", 4), "語");
    }

    #[test]
    fn ascii_passthrough_unchanged() {
        let s = "plain ascii error text";
        assert_eq!(truncate_head(s, 10), "plain asci");
        assert_eq!(truncate_tail(s, 10), "error text");
        assert_eq!(truncate_head("", 10), "");
        assert_eq!(truncate_tail("", 10), "");
    }
}
