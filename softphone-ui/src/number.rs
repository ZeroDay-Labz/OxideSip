//! Phone-number normalisation and matching shared by contacts, history and
//! the deny list — so "+1 (555) 123-4567", "15551234567" and
//! "sip:5551234567@pbx" all find the same contact.

/// Strips a number down to its dialable core: the user part of a SIP/tel
/// URI if one is given, then only digits, `*`, `#` and a leading `+`.
pub fn normalize(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("sip:")
        .or_else(|| raw.strip_prefix("sips:"))
        .or_else(|| raw.strip_prefix("tel:"))
        .unwrap_or(raw);
    let raw = raw.split('@').next().unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    for (i, c) in raw.chars().enumerate() {
        if c.is_ascii_digit() || c == '*' || c == '#' || (c == '+' && i == 0) {
            out.push(c);
        }
    }
    out
}

/// Whether two numbers refer to the same party. Exact match after
/// normalisation, or — for long (≥ 7 digit) numbers — one being a suffix of
/// the other, so a country-code-prefixed caller ID still matches the local
/// form saved in Contacts. Short extensions must match exactly.
pub fn matches(a: &str, b: &str) -> bool {
    let a = normalize(a);
    let b = normalize(b);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let a_digits = a.trim_start_matches('+');
    let b_digits = b.trim_start_matches('+');
    if a_digits == b_digits {
        return true;
    }
    let (short, long) = if a_digits.len() <= b_digits.len() {
        (a_digits, b_digits)
    } else {
        (b_digits, a_digits)
    };
    short.len() >= 7 && long.ends_with(short)
}

/// Deny-list semantics: an entry containing `@` or letters is matched as a
/// case-insensitive substring of the caller's URI or display name (block a
/// domain or a name); anything else is matched as a number.
pub fn denied(deny_list: &[String], display_name: Option<&str>, user: &str, uri: &str) -> bool {
    deny_list
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .any(|entry| {
            let textual = entry.contains('@') || entry.chars().any(|c| c.is_alphabetic());
            if textual {
                let needle = entry.to_lowercase();
                uri.to_lowercase().contains(&needle) || display_name.is_some_and(|n| n.to_lowercase().contains(&needle))
            } else {
                matches(entry, user)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_uris_and_punctuation() {
        assert_eq!(normalize("sip:1001@pbx.lan"), "1001");
        assert_eq!(normalize("+1 (555) 123-4567"), "+15551234567");
        assert_eq!(normalize("tel:+15551234567"), "+15551234567");
        assert_eq!(normalize("*97"), "*97");
    }

    #[test]
    fn matching_rules() {
        assert!(matches("1001", "sip:1001@pbx.lan"));
        assert!(matches("+15551234567", "5551234567"));
        assert!(matches("15551234567", "555-123-4567"));
        assert!(!matches("1001", "2001"));
        assert!(!matches("100", "1100"));
        assert!(!matches("", "1001"));
    }

    #[test]
    fn deny_list_semantics() {
        let list = vec![
            "spam.example".to_string(),
            "5551234567".to_string(),
            "Telemarketer".to_string(),
        ];
        assert!(denied(&list, None, "9000", "sip:9000@spam.example"));
        assert!(denied(&list, None, "+15551234567", "sip:+15551234567@pbx.lan"));
        assert!(denied(&list, Some("A Telemarketer"), "1234", "sip:1234@pbx.lan"));
        assert!(!denied(&list, Some("Alice"), "1001", "sip:1001@pbx.lan"));
    }
}
