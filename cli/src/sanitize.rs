pub fn sanitize_display(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let n = *c as u32;
            !((0x00..=0x1F).contains(&n) || n == 0x7F || (0x80..=0x9F).contains(&n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_c0_controls() {
        assert_eq!(sanitize_display("a\x00b\x01c\x1Fd"), "abcd");
    }

    #[test]
    fn strips_bel_esc_del() {
        assert_eq!(sanitize_display("a\x07b\x1Bc\x7Fd"), "abcd");
    }

    #[test]
    fn strips_c1_controls() {
        assert_eq!(sanitize_display("a\u{0080}b\u{009F}c"), "abc");
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(sanitize_display("héllo 🦀"), "héllo 🦀");
    }

    #[test]
    fn strips_osc_52_payload() {
        let payload = "\x1b]52;c;bW5hcnk=\x07";
        assert_eq!(sanitize_display(payload), "]52;c;bW5hcnk=");
    }

    #[test]
    fn idempotent() {
        let s = "safe-string_123";
        assert_eq!(sanitize_display(s), s);
        assert_eq!(sanitize_display(&sanitize_display(s)), s);
    }
}
