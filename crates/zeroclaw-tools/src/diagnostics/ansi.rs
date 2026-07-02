/// Strip ANSI CSI escape sequences from tool output.
pub fn strip_ansi(input: &str) -> String {
    if !input.as_bytes().contains(&0x1b) {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }

        if chars.peek() != Some(&'[') {
            out.push(ch);
            continue;
        }
        let _ = chars.next();

        let mut consumed = String::from("\u{1b}[");
        let mut terminated = false;
        for next in chars.by_ref() {
            consumed.push(next);
            if ('@'..='~').contains(&next) {
                terminated = true;
                break;
            }
        }

        if !terminated {
            out.push_str(&consumed);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_sgr_escape_sequences() {
        let input = "\u{1b}[0m\u{1b}[1m\u{1b}[38;5;9merror[E0425]\u{1b}[0m";
        assert_eq!(strip_ansi(input), "error[E0425]");
    }

    #[test]
    fn clean_text_is_unchanged() {
        assert_eq!(strip_ansi("plain output"), "plain output");
    }

    #[test]
    fn malformed_escape_is_kept() {
        assert_eq!(strip_ansi("before \u{1b}[31"), "before \u{1b}[31");
    }
}
