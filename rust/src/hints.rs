/// Check if the backslash at position `i` is itself escaped (preceded by an odd number of backslashes).
fn is_escaped_backslash(bytes: &[u8], i: usize) -> bool {
    let mut count = 0;
    let mut pos = i;
    while pos > 0 {
        pos -= 1;
        if bytes[pos] == b'\\' {
            count += 1;
        } else {
            break;
        }
    }
    count % 2 == 1
}

/// Detect if a literal search pattern looks like it was intended as a regex.
/// Returns a hint message if a regex-like pattern is detected, or None.
pub fn detect_regex_hint(pattern: &str) -> Option<String> {
    // Check each detector in order, return on first match
    if let Some(found) = detect_backslash_pipe(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_ere_or(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_shorthand_class(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_bre_group(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_char_class_range(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_quantifier_braces(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_lookaround(pattern) {
        return Some(format_hint(&found));
    }
    if let Some(found) = detect_escape_sequences(pattern) {
        return Some(format_hint(&found));
    }
    None
}

fn format_hint(found: &str) -> String {
    format!(
        "hint: pattern looks like a regex (found `{}`). Use -e to enable regex mode.",
        found
    )
}

/// `\|` — BRE-style OR
fn detect_backslash_pipe(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\' && bytes[i + 1] == b'|' && !is_escaped_backslash(bytes, i) {
            return Some("\\|".to_string());
        }
    }
    None
}

/// `(foo|bar)` — ERE-style OR with grouping
fn detect_ere_or(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    let mut depth = 0i32;
    let mut has_pipe_in_group = false;

    for (i, &b) in bytes.iter().enumerate() {
        // Skip escaped characters
        if i > 0 && bytes[i - 1] == b'\\' && !is_escaped_backslash(bytes, i - 1) {
            continue;
        }
        match b {
            b'(' => {
                depth += 1;
            }
            b')' => {
                if has_pipe_in_group && depth > 0 {
                    let start = pattern[..i]
                        .rfind('(')
                        .unwrap_or(0)
                        .min(i.saturating_sub(20));
                    let end = (i + 1).min(pattern.len());
                    return Some(pattern[start..end].to_string());
                }
                if depth > 0 {
                    depth -= 1;
                }
                has_pipe_in_group = false;
            }
            b'|' if depth > 0 => {
                has_pipe_in_group = true;
            }
            _ => {}
        }
    }
    None
}

/// `\d`, `\w`, `\s`, `\b`, `\D`, `\W`, `\S`, `\B`
fn detect_shorthand_class(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\' && !is_escaped_backslash(bytes, i) {
            let next = bytes[i + 1];
            if matches!(next, b'd' | b'D' | b'w' | b'W' | b's' | b'S' | b'b' | b'B') {
                return Some(format!("\\{}", next as char));
            }
        }
    }
    None
}

/// `\(` or `\)` — BRE-style grouping
fn detect_bre_group(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\' && !is_escaped_backslash(bytes, i) {
            if bytes[i + 1] == b'(' {
                return Some("\\(".to_string());
            }
            if bytes[i + 1] == b')' {
                return Some("\\)".to_string());
            }
        }
    }
    None
}

/// `[a-z]`, `[0-9]`, `[A-Z]` — character class with range
fn detect_char_class_range(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip escaped characters
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'[' {
            let bracket_start = i;
            i += 1;
            // Skip leading ^ or ]
            if i < bytes.len() && (bytes[i] == b'^' || bytes[i] == b']') {
                i += 1;
            }
            while i < bytes.len() && bytes[i] != b']' {
                // Look for a-z style range
                if i + 2 < bytes.len()
                    && bytes[i + 1] == b'-'
                    && bytes[i + 2] != b']'
                    && bytes[i].is_ascii_alphanumeric()
                    && bytes[i + 2].is_ascii_alphanumeric()
                {
                    let end = pattern[i..].find(']').map(|p| i + p + 1).unwrap_or(i + 3);
                    let end = end.min(pattern.len());
                    return Some(pattern[bracket_start..end].to_string());
                }
                i += 1;
            }
        }
        i += 1;
    }
    None
}

/// `{2,5}`, `{3}` — quantifier braces
fn detect_quantifier_braces(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'{' {
            // Check if contents look like a quantifier: digits, optional comma, optional digits
            let rest = &pattern[i..];
            if let Some(close) = rest.find('}') {
                let inner = &rest[1..close];
                if is_quantifier(inner) {
                    return Some(rest[..close + 1].to_string());
                }
            }
        }
    }
    None
}

fn is_quantifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if let Some((left, right)) = s.split_once(',') {
        // {n,} or {n,m}
        !left.is_empty()
            && left.chars().all(|c| c.is_ascii_digit())
            && (right.is_empty() || right.chars().all(|c| c.is_ascii_digit()))
    } else {
        // {n}
        s.chars().all(|c| c.is_ascii_digit())
    }
}

/// `(?:...)`, `(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)`
fn detect_lookaround(pattern: &str) -> Option<String> {
    for prefix in &["(?:", "(?=", "(?!", "(?<=", "(?<!"] {
        if pattern.contains(prefix) {
            return Some(prefix.to_string());
        }
    }
    None
}

/// `\n`, `\t` — escape sequences rarely intended as literal
fn detect_escape_sequences(pattern: &str) -> Option<String> {
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\'
            && !is_escaped_backslash(bytes, i)
            && matches!(bytes[i + 1], b'n' | b't')
        {
            return Some(format!("\\{}", bytes[i + 1] as char));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backslash_pipe() {
        assert!(detect_regex_hint(r"foo\|bar").is_some());
        assert!(detect_regex_hint(r"a\|b\|c").is_some());
    }

    #[test]
    fn test_ere_or() {
        assert!(detect_regex_hint("(get|set)Value").is_some());
        assert!(detect_regex_hint("(foo|bar|baz)").is_some());
        // No pipe in group = no hint
        assert!(detect_regex_hint("(foo)").is_none());
        // Pipe outside group = no hint (could be shell artifact)
        assert!(detect_regex_hint("foo|bar").is_none());
    }

    #[test]
    fn test_shorthand_classes() {
        assert!(detect_regex_hint(r"\d+").is_some());
        assert!(detect_regex_hint(r"\w+").is_some());
        assert!(detect_regex_hint(r"\s").is_some());
        assert!(detect_regex_hint(r"\b").is_some());
        assert!(detect_regex_hint(r"\D").is_some());
        assert!(detect_regex_hint(r"\W").is_some());
        assert!(detect_regex_hint(r"\S").is_some());
        assert!(detect_regex_hint(r"\B").is_some());
    }

    #[test]
    fn test_bre_group() {
        assert!(detect_regex_hint(r"\(foo\)").is_some());
        assert!(detect_regex_hint(r"\(").is_some());
    }

    #[test]
    fn test_char_class_range() {
        assert!(detect_regex_hint("[a-z]").is_some());
        assert!(detect_regex_hint("[0-9]").is_some());
        assert!(detect_regex_hint("[A-Z]+").is_some());
        // No range = no hint
        assert!(detect_regex_hint("[abc]").is_none());
    }

    #[test]
    fn test_quantifier_braces() {
        assert!(detect_regex_hint(r"\d{3}").is_some()); // also triggers \d
        assert!(detect_regex_hint("x{2,5}").is_some());
        assert!(detect_regex_hint("x{3,}").is_some());
        // Not a quantifier
        assert!(detect_regex_hint("{key: value}").is_none());
        assert!(detect_regex_hint("{}").is_none());
    }

    #[test]
    fn test_lookaround() {
        assert!(detect_regex_hint("(?:foo)").is_some());
        assert!(detect_regex_hint("(?=bar)").is_some());
        assert!(detect_regex_hint("(?!baz)").is_some());
        assert!(detect_regex_hint("(?<=x)").is_some());
        assert!(detect_regex_hint("(?<!y)").is_some());
    }

    #[test]
    fn test_escape_sequences() {
        assert!(detect_regex_hint(r"\n").is_some());
        assert!(detect_regex_hint(r"\t").is_some());
    }

    #[test]
    fn test_no_false_positives() {
        // Normal code patterns should NOT trigger hints
        assert!(detect_regex_hint("fn main").is_none());
        assert!(detect_regex_hint("pub struct Foo").is_none());
        assert!(detect_regex_hint("Vec<String>").is_none());
        assert!(detect_regex_hint("$HOME").is_none());
        assert!(detect_regex_hint("a + b").is_none());
        assert!(detect_regex_hint("array[0]").is_none());
        assert!(detect_regex_hint("x ^ y").is_none());
        assert!(detect_regex_hint("foo.bar").is_none());
        assert!(detect_regex_hint("Result<(), Error>").is_none());
        assert!(detect_regex_hint("import { foo }").is_none());
    }

    #[test]
    fn test_hint_message_format() {
        let hint = detect_regex_hint(r"foo\|bar").unwrap();
        assert!(hint.starts_with("hint:"));
        assert!(hint.contains("-e"));
        assert!(hint.contains(r"\|"));
    }

    #[test]
    fn test_escaped_backslash_no_false_positive() {
        // \\d = literal backslash + 'd', NOT \d shorthand
        assert!(detect_regex_hint(r"\\d").is_none());
        // \\| = literal backslash + '|', NOT \| OR
        assert!(detect_regex_hint(r"\\|").is_none());
        // \\( = literal backslash + '(', NOT \( BRE group
        assert!(detect_regex_hint(r"\\(").is_none());
        // \\n = literal backslash + 'n', NOT \n newline
        assert!(detect_regex_hint(r"\\n").is_none());
        // \\\d = escaped backslash + \d shorthand (should trigger)
        assert!(detect_regex_hint(r"\\\d").is_some());
    }
}
