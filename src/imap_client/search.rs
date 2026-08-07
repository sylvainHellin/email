/// Structured search criteria for IMAP queries.
#[derive(Default)]
pub struct FetchCriteria {
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub text: Option<String>,
    /// RFC 5322 Message-ID to look up. Stored with or without angle brackets;
    /// every consumer normalizes through [`normalize_message_id`].
    pub message_id: Option<String>,
    /// Routing directive: which mailbox to search. Not an IMAP search criterion.
    pub in_mailbox: Option<String>,
}

/// Normalize a Message-ID for comparison: trim whitespace and strip one layer of
/// angle brackets, so `<a@b>` and `a@b` compare equal. Idempotent.
pub fn normalize_message_id(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed)
}

/// Canonical wire form of a Message-ID, always angle-bracketed: `<a@b>`.
/// Servers store the header with the brackets, so queries must carry them.
pub fn bracketed_message_id(raw: &str) -> String {
    format!("<{}>", normalize_message_id(raw))
}

/// The `HEADER Message-ID` search term for one Message-ID, with the value
/// quoted as an IMAP string literal.
///
/// A quoted string escapes exactly two characters (RFC 3501 section 4.3):
/// `\` and `"`. A Message-ID carrying either one interpolated raw would close
/// the literal early and leave the server parsing the remainder as search
/// keywords, which is a malformed command rather than a match. The value comes
/// from a header or from a self-authored draft, so this is hygiene rather than
/// a live failure, but every site that names a message this way should quote
/// it the same way.
///
/// The value is passed through as given rather than bracketed: the callers
/// that need the bracket discipline go through [`build_imap_search_query`],
/// and the per-message ops search for the Message-ID the store holds.
pub(crate) fn message_id_search_term(message_id: &str) -> String {
    let escaped = message_id.replace('\\', "\\\\").replace('"', "\\\"");
    format!("HEADER Message-ID \"{escaped}\"")
}

/// Drop results whose Message-ID is not exactly `criteria.message_id`.
///
/// Neither backend gives an exact match on its own: IMAP `HEADER` is a substring
/// match over the header text, and Graph falls back to a fuzzy `$search` when
/// `$filter` is rejected. Comparison ignores ASCII case because servers are not
/// consistent about the domain part; it is still equality, never a substring.
/// A no-op when no Message-ID was requested.
pub fn retain_exact_message_id(emails: &mut Vec<crate::parse::FetchedEmail>, criteria: &FetchCriteria) {
    let Some(ref wanted) = criteria.message_id else {
        return;
    };
    let wanted = normalize_message_id(wanted);
    emails.retain(|email| {
        email
            .message_id
            .as_deref()
            .is_some_and(|mid| normalize_message_id(mid).eq_ignore_ascii_case(wanted))
    });
}

pub(crate) fn build_imap_search_query(criteria: &FetchCriteria) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref from) = criteria.from {
        parts.push(format!("FROM \"{}\"", from));
    }
    if let Some(ref to) = criteria.to {
        parts.push(format!("TO \"{}\"", to));
    }
    if let Some(ref cc) = criteria.cc {
        parts.push(format!("CC \"{}\"", cc));
    }
    if let Some(ref subject) = criteria.subject {
        parts.push(format!("SUBJECT \"{}\"", subject));
    }
    if let Some(ref body) = criteria.body {
        parts.push(format!("BODY \"{}\"", body));
    }
    if let Some(ref message_id) = criteria.message_id {
        // Always bracketed: `HEADER` is a substring match, and the brackets are
        // what stop `<abc@x>` from also matching `<prefix-abc@x>`. Exactness is
        // finished off by retain_exact_message_id on the results.
        parts.push(format!(
            "HEADER \"Message-ID\" \"{}\"",
            bracketed_message_id(message_id)
        ));
    }
    if let Some(ref since) = criteria.since {
        if let Some(imap_date) = parse_date_to_imap(since) {
            parts.push(format!("SINCE {}", imap_date));
        }
    }
    if let Some(ref before) = criteria.before {
        if let Some(imap_date) = parse_date_to_imap(before) {
            parts.push(format!("BEFORE {}", imap_date));
        }
    }

    if let Some(ref text) = criteria.text {
        parts.push(format!("TEXT \"{}\"", text));
    }

    if parts.is_empty() {
        "ALL".to_string()
    } else {
        parts.join(" ")
    }
}

/// Parse a user search string into structured FetchCriteria.
///
/// Recognized prefixes: from:, to:, cc:, subject:, body:, since:, before:,
/// message-id:, in:
/// Quoted values supported: from:"John Doe"
/// Bare text (no prefix) becomes a TEXT search.
pub fn parse_search_query(input: &str) -> FetchCriteria {
    let mut criteria = FetchCriteria {
        from: None,
        to: None,
        cc: None,
        subject: None,
        body: None,
        since: None,
        before: None,
        text: None,
        message_id: None,
        in_mailbox: None,
    };

    let mut remaining = Vec::new();
    let mut chars = input.chars().peekable();

    while chars.peek().is_some() {
        while chars.peek() == Some(&' ') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let rest: String = chars.clone().collect();
        let lower_rest = rest.to_lowercase();

        let mut matched = false;
        for (prefix, setter) in [
            ("from:", 0u8),
            ("to:", 1),
            ("cc:", 2),
            ("subject:", 3),
            ("body:", 4),
            ("since:", 5),
            ("before:", 6),
            ("in:", 7),
            ("message-id:", 8),
        ] {
            if lower_rest.starts_with(prefix) {
                for _ in 0..prefix.len() {
                    chars.next();
                }
                let value = extract_search_value(&mut chars);
                match setter {
                    0 => criteria.from = Some(value),
                    1 => criteria.to = Some(value),
                    2 => criteria.cc = Some(value),
                    3 => criteria.subject = Some(value),
                    4 => criteria.body = Some(value),
                    5 => criteria.since = Some(value),
                    6 => criteria.before = Some(value),
                    7 => criteria.in_mailbox = Some(value),
                    8 => criteria.message_id = Some(value),
                    _ => unreachable!(),
                }
                matched = true;
                break;
            }
        }

        if !matched {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                word.push(c);
                chars.next();
            }
            if !word.is_empty() {
                remaining.push(word);
            }
        }
    }

    if !remaining.is_empty() {
        criteria.text = Some(remaining.join(" "));
    }

    criteria
}

fn extract_search_value(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    while chars.peek() == Some(&' ') {
        chars.next();
    }

    if chars.peek() == Some(&'"') {
        chars.next();
        let mut value = String::new();
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            value.push(c);
        }
        value
    } else {
        let mut value = String::new();
        while let Some(&c) = chars.peek() {
            if c == ' ' {
                break;
            }
            value.push(c);
            chars.next();
        }
        value
    }
}

pub(crate) fn parse_date_to_imap(date_str: &str) -> Option<String> {
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year = parts[0];
    let month = match parts[1] {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => return None,
    };
    let day: u32 = parts[2].parse().ok()?;
    Some(format!("{}-{}-{}", day, month, year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_date_to_imap_valid() {
        assert_eq!(
            parse_date_to_imap("2024-12-25"),
            Some("25-Dec-2024".to_string())
        );
        assert_eq!(
            parse_date_to_imap("2026-01-01"),
            Some("1-Jan-2026".to_string())
        );
        assert_eq!(
            parse_date_to_imap("2023-06-15"),
            Some("15-Jun-2023".to_string())
        );
    }

    #[test]
    fn test_parse_date_to_imap_invalid_format() {
        assert_eq!(parse_date_to_imap("2024/12/25"), None);
        assert_eq!(parse_date_to_imap("Dec 25 2024"), None);
        assert_eq!(parse_date_to_imap("25-Dec-2024"), None);
        assert_eq!(parse_date_to_imap("2024-1-5"), None);
    }

    #[test]
    fn test_parse_date_to_imap_invalid_month() {
        assert_eq!(parse_date_to_imap("2024-13-01"), None);
        assert_eq!(parse_date_to_imap("2024-00-01"), None);
        assert_eq!(parse_date_to_imap("2024-99-01"), None);
    }

    #[test]
    fn test_parse_date_to_imap_invalid_day() {
        // parse_date_to_imap only validates that day is a valid u32, not range.
        // Invalid days will be rejected by the IMAP server at query time.
        assert_eq!(parse_date_to_imap("2024-12-00"), Some("0-Dec-2024".to_string()));
        assert_eq!(parse_date_to_imap("2024-12-32"), Some("32-Dec-2024".to_string()));
        assert_eq!(parse_date_to_imap("2024-12-ab"), None);
    }

    #[test]
    fn test_parse_date_to_imap_empty() {
        assert_eq!(parse_date_to_imap(""), None);
        assert_eq!(parse_date_to_imap("2024"), None);
        assert_eq!(parse_date_to_imap("2024-12"), None);
    }

    #[test]
    fn test_build_imap_search_query_empty() {
        let criteria = FetchCriteria {
            from: None,
            to: None,
            cc: None,
            subject: None,
            body: None,
            since: None,
            before: None,
            text: None,
            message_id: None,
            in_mailbox: None,
        };
        assert_eq!(build_imap_search_query(&criteria), "ALL");
    }

    #[test]
    fn test_build_imap_search_query_single() {
        let mut criteria = FetchCriteria::default();
        criteria.from = Some("alice".to_string());
        assert_eq!(build_imap_search_query(&criteria), "FROM \"alice\"");

        let mut criteria = FetchCriteria::default();
        criteria.subject = Some("invoice".to_string());
        assert_eq!(
            build_imap_search_query(&criteria),
            "SUBJECT \"invoice\""
        );
    }

    #[test]
    fn test_build_imap_search_query_multiple() {
        let mut criteria = FetchCriteria::default();
        criteria.from = Some("alice".to_string());
        criteria.to = Some("bob".to_string());
        criteria.subject = Some("invoice".to_string());
        let query = build_imap_search_query(&criteria);
        assert!(query.contains("FROM \"alice\""));
        assert!(query.contains("TO \"bob\""));
        assert!(query.contains("SUBJECT \"invoice\""));
        let parts: Vec<&str> = query.split(' ').collect();
        assert_eq!(parts.len(), 6);
    }

    #[test]
    fn test_build_imap_search_query_date_criteria() {
        let mut criteria = FetchCriteria::default();
        criteria.since = Some("2024-12-01".to_string());
        criteria.before = Some("2024-12-31".to_string());
        let query = build_imap_search_query(&criteria);
        assert!(query.contains("SINCE 1-Dec-2024"));
        assert!(query.contains("BEFORE 31-Dec-2024"));
    }

    #[test]
    fn test_build_imap_search_query_text() {
        let mut criteria = FetchCriteria::default();
        criteria.text = Some("urgent meeting".to_string());
        assert_eq!(
            build_imap_search_query(&criteria),
            "TEXT \"urgent meeting\""
        );
    }

    #[test]
    fn test_parse_search_query_from() {
        let criteria = parse_search_query("from:alice@example.com");
        assert_eq!(criteria.from, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_search_query_to() {
        let criteria = parse_search_query("to:bob@example.org");
        assert_eq!(criteria.to, Some("bob@example.org".to_string()));
    }

    #[test]
    fn test_parse_search_query_cc() {
        let criteria = parse_search_query("cc:secret@example.net");
        assert_eq!(criteria.cc, Some("secret@example.net".to_string()));
    }

    #[test]
    fn test_parse_search_query_subject() {
        let criteria = parse_search_query("subject:invoice");
        assert_eq!(criteria.subject, Some("invoice".to_string()));
    }

    #[test]
    fn test_parse_search_query_body() {
        let criteria = parse_search_query("body:contract");
        assert_eq!(criteria.body, Some("contract".to_string()));
    }

    #[test]
    fn test_parse_search_query_since_before() {
        let criteria = parse_search_query("since:2024-12-01 before:2024-12-31");
        assert_eq!(criteria.since, Some("2024-12-01".to_string()));
        assert_eq!(criteria.before, Some("2024-12-31".to_string()));
    }

    #[test]
    fn test_parse_search_query_in_mailbox() {
        let criteria = parse_search_query("in:INBOX from:alice");
        assert_eq!(criteria.in_mailbox, Some("INBOX".to_string()));
        assert_eq!(criteria.from, Some("alice".to_string()));
    }

    #[test]
    fn test_parse_search_query_bare_text() {
        let criteria = parse_search_query("urgent meeting");
        assert_eq!(criteria.text, Some("urgent meeting".to_string()));
    }

    #[test]
    fn test_parse_search_query_quoted_value() {
        let criteria = parse_search_query("from:\"Alice Smith\" subject:report");
        assert_eq!(criteria.from, Some("Alice Smith".to_string()));
        assert_eq!(criteria.subject, Some("report".to_string()));
    }

    #[test]
    fn test_parse_search_query_case_insensitive_prefix() {
        let criteria = parse_search_query("FROM:alice SUBJECT:report");
        assert_eq!(criteria.from, Some("alice".to_string()));
        assert_eq!(criteria.subject, Some("report".to_string()));
    }

    #[test]
    fn test_parse_search_query_multiple_bare_words() {
        let criteria = parse_search_query("urgent meeting notes");
        assert_eq!(criteria.text, Some("urgent meeting notes".to_string()));
    }

    #[test]
    fn test_parse_search_query_mixed_prefix_and_bare() {
        let criteria = parse_search_query("from:alice urgent");
        assert_eq!(criteria.from, Some("alice".to_string()));
        assert_eq!(criteria.text, Some("urgent".to_string()));
    }

    #[test]
    fn test_normalize_message_id_strips_one_bracket_layer() {
        assert_eq!(normalize_message_id("<abc@example.com>"), "abc@example.com");
        assert_eq!(normalize_message_id("abc@example.com"), "abc@example.com");
        assert_eq!(normalize_message_id("  <abc@example.com> "), "abc@example.com");
        // Half-bracketed input is left alone rather than silently mangled.
        assert_eq!(normalize_message_id("<abc@example.com"), "<abc@example.com");
    }

    #[test]
    fn a_search_term_escapes_the_two_characters_a_quoted_string_has() {
        assert_eq!(
            message_id_search_term("<a@b>"),
            "HEADER Message-ID \"<a@b>\""
        );
        assert_eq!(
            message_id_search_term("<a\\qb@x>"),
            "HEADER Message-ID \"<a\\\\qb@x>\""
        );
        assert_eq!(
            message_id_search_term("<a\"b@x>"),
            "HEADER Message-ID \"<a\\\"b@x>\""
        );
        // The backslash pass runs first, so an escaped quote is not
        // double-escaped into a literal backslash plus a bare quote.
        assert_eq!(
            message_id_search_term("<a\\\"b@x>"),
            "HEADER Message-ID \"<a\\\\\\\"b@x>\""
        );
    }

    #[test]
    fn test_bracketed_message_id_is_idempotent() {
        assert_eq!(bracketed_message_id("abc@example.com"), "<abc@example.com>");
        assert_eq!(
            bracketed_message_id("<abc@example.com>"),
            "<abc@example.com>"
        );
    }

    #[test]
    fn test_parse_search_query_message_id() {
        let bare = parse_search_query("message-id:abc123@example.com");
        assert_eq!(bare.message_id, Some("abc123@example.com".to_string()));
        // Angle brackets survive the parser; normalization happens downstream.
        let bracketed = parse_search_query("message-id:<abc123@example.com>");
        assert_eq!(bracketed.message_id, Some("<abc123@example.com>".to_string()));
        // Prefix matching is case-insensitive like every other prefix.
        let upper = parse_search_query("MESSAGE-ID:<abc123@example.com>");
        assert_eq!(upper.message_id, Some("<abc123@example.com>".to_string()));
    }

    #[test]
    fn test_parse_search_query_message_id_not_confused_with_other_prefixes() {
        let criteria = parse_search_query("in:Archive message-id:<a@b> from:alice");
        assert_eq!(criteria.in_mailbox, Some("Archive".to_string()));
        assert_eq!(criteria.message_id, Some("<a@b>".to_string()));
        assert_eq!(criteria.from, Some("alice".to_string()));
        assert_eq!(criteria.text, None);
    }

    #[test]
    fn test_build_imap_search_query_message_id_always_bracketed() {
        let criteria = FetchCriteria {
            message_id: Some("abc123@example.com".to_string()),
            ..Default::default()
        };
        assert_eq!(
            build_imap_search_query(&criteria),
            "HEADER \"Message-ID\" \"<abc123@example.com>\""
        );

        let already = FetchCriteria {
            message_id: Some("<abc123@example.com>".to_string()),
            ..Default::default()
        };
        assert_eq!(
            build_imap_search_query(&already),
            "HEADER \"Message-ID\" \"<abc123@example.com>\""
        );
    }

    fn email_with_id(id: Option<&str>) -> crate::parse::FetchedEmail {
        crate::parse::FetchedEmail {
            from: String::new(),
            to: String::new(),
            cc: None,
            subject: String::new(),
            date: String::new(),
            body_text: String::new(),
            html_body: None,
            has_attachments: false,
            message_id: id.map(|s| s.to_string()),
            attachments: Vec::new(),
            flags: Default::default(),
            calendar_ics: None,
            event: None,
        }
    }

    #[test]
    fn test_retain_exact_message_id_drops_substring_matches() {
        let criteria = FetchCriteria {
            message_id: Some("<abc@example.com>".to_string()),
            ..Default::default()
        };

        let mut emails = vec![
            email_with_id(Some("<prefix-abc@example.com>")),
            email_with_id(Some("<abc@example.com.evil.net>")),
            email_with_id(Some("<abc@example.com>")),
            email_with_id(None),
        ];
        retain_exact_message_id(&mut emails, &criteria);

        assert_eq!(emails.len(), 1);
        assert_eq!(
            emails[0].message_id,
            Some("<abc@example.com>".to_string())
        );
    }

    #[test]
    fn test_retain_exact_message_id_matches_across_bracket_and_case_variants() {
        let criteria = FetchCriteria {
            message_id: Some("abc@Example.COM".to_string()),
            ..Default::default()
        };

        let mut emails = vec![email_with_id(Some("<abc@example.com>"))];
        retain_exact_message_id(&mut emails, &criteria);
        assert_eq!(emails.len(), 1);
    }

    #[test]
    fn test_retain_exact_message_id_is_a_noop_without_criteria() {
        let criteria = FetchCriteria::default();
        let mut emails = vec![email_with_id(Some("<a@b>")), email_with_id(None)];
        retain_exact_message_id(&mut emails, &criteria);
        assert_eq!(emails.len(), 2);
    }
}
