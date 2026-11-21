//! Native IMAP threading with bounded metadata. No raw user input is ever
//! interpolated into a command. io-imap 0.12 exposes a byte-exact raw exchange
//! for extensions not represented by its FETCH types (Gmail).
use anyhow::{Context, Result, ensure};
use std::collections::{HashMap, HashSet};

pub(super) const MAX_THREAD_MESSAGES: usize = 20_000;
const MAX_THREAD_REPLY_BYTES: usize = 4 * 1024 * 1024;
const TAG: &str = "jercThread";

pub(super) fn command(body: &str) -> String {
    format!("{TAG} {body}\r\n")
}

/// Raw returns one response per entry, NOT a single concatenated string.
/// Only the expected tagged OK can establish successful completion.
pub(super) fn checked_response(response: &[Vec<u8>]) -> Result<Vec<&str>> {
    let mut bytes = 0usize;
    let mut ok = false;
    let mut lines = Vec::new();
    for line in response {
        bytes = bytes.saturating_add(line.len());
        ensure!(
            bytes <= MAX_THREAD_REPLY_BYTES,
            "IMAP threading response too large"
        );
        let line = std::str::from_utf8(line)
            .context("invalid IMAP threading response")?
            .trim_end_matches(['\r', '\n']);
        if let Some(tail) = line.strip_prefix(TAG).and_then(|s| s.strip_prefix(' ')) {
            ensure!(
                !ok && tail
                    .split_ascii_whitespace()
                    .next()
                    .is_some_and(|s| s.eq_ignore_ascii_case("OK")),
                "IMAP threading command rejected"
            );
            ok = true;
        } else if let Some(stripped) = line.strip_prefix("* ") {
            ensure!(!ok, "unexpected response after IMAP threading completion");
            ensure!(
                !stripped
                    .split_ascii_whitespace()
                    .next()
                    .is_some_and(|s| s.eq_ignore_ascii_case("BYE")),
                "IMAP connection closed during threading"
            );
            lines.push(line);
        }
    }
    ensure!(ok, "missing IMAP threading completion");
    Ok(lines)
}

pub(super) fn capabilities(lines: &[&str]) -> (bool, bool) {
    let mut references = false;
    let mut gmail = false;
    for line in lines {
        let mut words = line.split_ascii_whitespace();
        if words.next() == Some("*")
            && words
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("CAPABILITY"))
        {
            for word in words {
                references |= word.eq_ignore_ascii_case("THREAD=REFERENCES");
                gmail |= word.eq_ignore_ascii_case("X-GM-EXT-1");
            }
        }
    }
    (references, gmail)
}

/// Flatten each RFC 5256 top-level tree, preserving membership rather than
/// confusing child branches for unrelated conversations. Iterative parsing
/// avoids stack overflow on arbitrarily nested server input.
pub(super) fn references_groups(lines: &[&str]) -> Result<Vec<Vec<u32>>> {
    let mut groups = Vec::new();
    let mut seen = HashSet::new();
    let mut found = false;
    for line in lines {
        let mut words = line.splitn(3, ' ');
        if words.next() != Some("*")
            || !words
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("THREAD"))
        {
            continue;
        }
        ensure!(!found, "duplicate THREAD response");
        found = true;
        let mut depth = 0usize;
        let mut group = Vec::new();
        let mut chars = words.next().unwrap_or("").bytes().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                b'(' => {
                    depth += 1;
                    ensure!(depth <= 1024, "IMAP thread too deeply nested");
                }
                b')' => {
                    ensure!(depth > 0, "unbalanced IMAP thread");
                    depth -= 1;
                    if depth == 0 {
                        ensure!(!group.is_empty(), "empty IMAP thread");
                        groups.push(std::mem::take(&mut group));
                    }
                }
                b'0'..=b'9' => {
                    ensure!(depth > 0, "UID outside IMAP thread");
                    let mut uid = u32::from(ch - b'0');
                    while let Some(next @ b'0'..=b'9') = chars.peek().copied() {
                        chars.next();
                        uid = uid
                            .checked_mul(10)
                            .and_then(|n| n.checked_add(u32::from(next - b'0')))
                            .context("invalid thread UID")?;
                    }
                    ensure!(
                        uid > 0 && seen.insert(uid) && seen.len() <= MAX_THREAD_MESSAGES,
                        "invalid, repeated or excessive thread UID"
                    );
                    group.push(uid);
                }
                b' ' | b'\t' => {}
                _ => anyhow::bail!("malformed IMAP THREAD response"),
            }
        }
        ensure!(depth == 0, "unbalanced IMAP thread");
    }
    // RFC 5256 permits no untagged THREAD response for no matches.
    Ok(groups)
}

/// Only accept FETCH records containing exactly the two requested numeric
/// fields. Unsolicited flag-only updates are ignored, never used as IDs.
pub(super) fn gmail_groups(lines: &[&str]) -> Result<HashMap<u64, Vec<u32>>> {
    let mut groups = HashMap::<u64, Vec<u32>>::new();
    let mut seen = HashSet::new();
    for line in lines {
        let words: Vec<_> = line
            .split(|c: char| c.is_ascii_whitespace() || c == '(' || c == ')')
            .filter(|s| !s.is_empty())
            .collect();
        if words.len() < 3
            || words[0] != "*"
            || !words[2].eq_ignore_ascii_case("FETCH")
            || !words.iter().any(|w| w.eq_ignore_ascii_case("X-GM-THRID"))
        {
            continue;
        }
        ensure!(
            words.len() == 7 && words[1].parse::<u32>().is_ok(),
            "invalid Gmail thread FETCH"
        );
        let mut uid = None;
        let mut thread = None;
        for pair in words[3..].as_chunks::<2>().0 {
            if pair[0].eq_ignore_ascii_case("UID") {
                uid = Some(pair[1].parse::<u32>()?);
            } else if pair[0].eq_ignore_ascii_case("X-GM-THRID") {
                thread = Some(pair[1].parse::<u64>()?);
            } else {
                anyhow::bail!("unexpected Gmail thread field");
            }
        }
        let uid = uid.filter(|id| *id > 0).context("missing Gmail UID")?;
        let thread = thread
            .filter(|id| *id > 0)
            .context("missing Gmail thread ID")?;
        ensure!(
            seen.insert(uid) && seen.len() <= MAX_THREAD_MESSAGES,
            "duplicate or excessive Gmail thread UID"
        );
        groups.entry(thread).or_default().push(uid);
    }
    Ok(groups)
}

pub(super) fn search_uids(lines: &[&str]) -> Result<Vec<u32>> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    let mut found = false;
    for line in lines {
        let mut words = line.split_ascii_whitespace();
        if words.next() != Some("*")
            || !words
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("SEARCH"))
        {
            continue;
        }
        ensure!(!found, "duplicate SEARCH response");
        found = true;
        for word in words {
            let uid: u32 = word.parse()?;
            ensure!(
                uid != 0 && seen.insert(uid) && seen.len() <= MAX_THREAD_MESSAGES,
                "invalid or excessive search UID"
            );
            ids.push(uid);
        }
    }
    ensure!(found, "missing Gmail SEARCH response");
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_rfc_groups_and_invalid_responses() {
        assert_eq!(
            references_groups(&["* THREAD (2)(3 6 (4 23)(44 7 96))"]).unwrap(),
            vec![vec![2], vec![3, 6, 4, 23, 44, 7, 96]]
        );
        for invalid in [
            "* THREAD (1)(1)",
            "* THREAD (0)",
            "* THREAD (2",
            "* THREAD 3",
            "* THREAD (4294967296)",
        ] {
            assert!(references_groups(&[invalid]).is_err());
        }
    }
    #[test]
    fn gmail_ids_are_unsigned_64_bit_and_field_order_independent() {
        let groups = gmail_groups(&[
            "* 1 FETCH (UID 42 X-GM-THRID 18446744073709551615)",
            "* 2 FETCH (X-GM-THRID 18446744073709551615 UID 43)",
            "* 3 FETCH (FLAGS (\\Seen))",
        ])
        .unwrap();
        assert_eq!(groups[&u64::MAX], vec![42, 43]);
        assert!(gmail_groups(&["* 1 FETCH (UID 42 X-GM-THRID 0)"]).is_err());
        assert_eq!(search_uids(&["* SEARCH 42 43"]).unwrap(), vec![42, 43]);
    }
    #[test]
    fn raw_completion_is_not_inferred_from_untagged_data() {
        assert!(checked_response(&[b"* THREAD (1)\r\n".to_vec()]).is_err());
        assert!(checked_response(&[b"jercThread NO rejected\r\n".to_vec()]).is_err());
        let response = vec![
            b"* CAPABILITY IMAP4rev1 thread=references X-GM-EXT-1\r\n".to_vec(),
            b"jercThread OK done\r\n".to_vec(),
        ];
        assert_eq!(
            capabilities(&checked_response(&response).unwrap()),
            (true, true)
        );
    }
}
