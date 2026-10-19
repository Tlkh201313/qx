//! Secret redaction for output that goes back to the model.
//!
//! Deliberately conservative: only well-known credential shapes and
//! `key=value` assignments with secret-looking names are touched, so ordinary
//! logs and hashes survive.

const KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "secret_access_key",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "access-key",
    "client_secret",
    "client-secret",
    "private_key",
    "connectionstring",
];

const PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),
    ("sk_live_", 16),
    ("sk_test_", 16),
    ("pk_live_", 16),
    ("pk_test_", 16),
    ("rk_live_", 16),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("AKIA", 16),
    ("ASIA", 16),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xoxa-", 10),
    ("xoxs-", 10),
    ("AIza", 30),
    ("ya29.", 30),
    ("glpat-", 20),
    ("hf_", 20),
    ("npm_", 20),
    ("dop_v1_", 40),
    ("sntrys_", 20),
    ("SG.", 30),
    ("eyJ", 40),
];

pub fn redact(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut count = 0usize;
    let mut in_key_block = false;
    let lines: Vec<&str> = input.split('\n').collect();

    for (i, line) in lines.iter().enumerate() {
        let upper = line.to_ascii_uppercase();
        let has_begin = upper.contains("BEGIN") && upper.contains("PRIVATE KEY");
        let has_end = upper.contains("END") && upper.contains("PRIVATE KEY");

        if in_key_block {
            if has_end {
                in_key_block = false;
            }
            if i + 1 < lines.len() {
                out.push('\n');
            }
            continue;
        }

        if has_begin {
            out.push_str("[REDACTED PRIVATE KEY]");
            count += 1;
            // A JSON-encoded key keeps BEGIN and END on one line; a PEM dump
            // spreads them across lines and must stay suppressed until END.
            if !has_end {
                in_key_block = true;
            }
            if i + 1 < lines.len() {
                out.push('\n');
            }
            continue;
        }

        // Bearer first: masking the word "Bearer" as a key value would hide the
        // anchor and leave the actual token exposed.
        let (masked, n1) = mask_bearer(line);
        let (masked, n2) = mask_conn_strings(&masked);
        let (masked, n3) = mask_key_values(&masked);
        let (masked, n4) = mask_tokens(&masked);
        count += n1 + n2 + n3 + n4;
        out.push_str(&masked);
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    (out, count)
}

/// `postgres://user:password@host` -> `postgres://user:***@host`.
fn mask_conn_strings(line: &str) -> (String, usize) {
    let bytes = line.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut search = 0usize;
    while let Some(pos) = line[search..].find("://") {
        let after = search + pos + 3;
        let mut at = None;
        let mut k = after;
        while k < bytes.len() {
            let b = bytes[k];
            if b == b'@' {
                at = Some(k);
                break;
            }
            if b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b'|' | b'&') {
                break;
            }
            k += 1;
        }
        if let Some(a) = at {
            if let Some(colon) = line[after..a].rfind(':') {
                let start = after + colon + 1;
                if a > start {
                    ranges.push((start, a));
                }
            }
        }
        search = after;
    }
    mask_ranges(line, ranges)
}

fn mask_key_values(line: &str) -> (String, usize) {
    let lower = line.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    for key in KEYS {
        let mut from = 0usize;
        while from < lower.len() {
            let Some(pos) = lower[from..].find(key) else {
                break;
            };
            let abs = from + pos;
            let before_ok = abs == 0 || !line.as_bytes()[abs - 1].is_ascii_alphanumeric();
            let mut j = abs + key.len();
            while j < line.len() && (line.as_bytes()[j] == b' ' || line.as_bytes()[j] == b'\t') {
                j += 1;
            }
            if before_ok && j < line.len() && (line.as_bytes()[j] == b'=' || line.as_bytes()[j] == b':') {
                j += 1;
                while j < line.len()
                    && matches!(line.as_bytes()[j], b' ' | b'\t' | b'"' | b'\'')
                {
                    j += 1;
                }
                let start = j;
                while j < line.len() {
                    let b = line.as_bytes()[j];
                    if matches!(b, b' ' | b'\t' | b',' | b';' | b'"' | b'\'' | b'\r' | b'\n') {
                        break;
                    }
                    j += 1;
                }
                if j - start >= 4 {
                    ranges.push((start, j));
                }
            }
            from = abs + key.len();
        }
    }

    mask_ranges(line, ranges)
}

fn mask_ranges(line: &str, mut ranges: Vec<(usize, usize)>) -> (String, usize) {
    if ranges.is_empty() {
        return (line.to_string(), 0);
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                if e > last.1 {
                    last.1 = e;
                }
                continue;
            }
        }
        merged.push((s, e));
    }
    let mut out = String::new();
    let mut pos = 0usize;
    for (s, e) in &merged {
        out.push_str(&line[pos..*s]);
        out.push_str("***");
        pos = *e;
    }
    out.push_str(&line[pos..]);
    (out, merged.len())
}

fn mask_tokens(line: &str) -> (String, usize) {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut i = 0usize;
    let mut count = 0usize;

    while i < bytes.len() {
        if line.is_char_boundary(i) {
            if let Some((plen, min)) = prefix_at(bytes, i) {
                let mut j = i + plen;
                while j < bytes.len() && is_token_byte(bytes[j]) {
                    j += 1;
                }
                if j - (i + plen) >= min {
                    out.push_str(&line[i..i + plen]);
                    out.push_str("***");
                    count += 1;
                    i = j;
                    continue;
                }
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

fn prefix_at(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    for (p, min) in PREFIXES {
        let pb = p.as_bytes();
        if i + pb.len() <= bytes.len() && &bytes[i..i + pb.len()] == pb {
            return Some((pb.len(), *min));
        }
    }
    None
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn mask_bearer(line: &str) -> (String, usize) {
    let lower = line.to_ascii_lowercase();
    let Some(pos) = lower.find("bearer ") else {
        return (line.to_string(), 0);
    };
    let start = pos + 7;
    let bytes = line.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_graphic() && !matches!(bytes[end], b'"' | b'\'') {
        end += 1;
    }
    if end - start >= 12 {
        let mut out = String::with_capacity(line.len());
        out.push_str(&line[..start]);
        out.push_str("***");
        out.push_str(&line[end..]);
        (out, 1)
    } else {
        (line.to_string(), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_api_keys() {
        let (s, n) = redact("OPENAI_KEY=sk-abcdefghijklmnopqrstuvwxyz0123\n");
        assert!(s.contains("OPENAI_KEY="));
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_password_assignment() {
        let (s, n) = redact("password=hunter2hunter2\nuser=bob\n");
        assert!(!s.contains("hunter2hunter2"));
        assert!(s.contains("user=bob"));
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_bearer_tokens() {
        let (s, n) = redact("Authorization: Bearer abcdefghijklmnopqrstuvwxyz\n");
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(n >= 1);
    }

    #[test]
    fn masks_private_key_blocks() {
        let input = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\nafter\n";
        let (s, n) = redact(input);
        assert!(!s.contains("MIIEow"));
        assert!(s.contains("before"));
        assert!(s.contains("after"));
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_single_line_json_private_key() {
        let input =
            "{\"private_key\": \"-----BEGIN PRIVATE KEY-----\\nMIIEvQ\\n-----END PRIVATE KEY-----\\n\"}\nnext\n";
        let (s, n) = redact(input);
        assert!(!s.contains("MIIEvQ"));
        assert!(s.contains("next"));
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_connection_string_passwords() {
        let (s, n) = redact("postgres://alice:s3cr3tpassword@db.example:5432/app\n");
        assert!(s.contains("alice"));
        assert!(!s.contains("s3cr3tpassword"));
        assert_eq!(n, 1);
    }

    #[test]
    fn masks_jwts() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let (s, _) = redact(&format!("token={jwt}\n"));
        assert!(!s.contains(jwt));
    }

    #[test]
    fn leaves_normal_output_alone() {
        let input = "Compiling tokenizers v0.15.2\nprice: 10.50\nrisk-assessment done\n";
        let (s, n) = redact(input);
        assert_eq!(s, input);
        assert_eq!(n, 0);
    }
}
