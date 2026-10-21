//! Secret redaction for output that goes back to the model.
//!
//! Deliberately conservative: only well-known credential shapes and
//! `key=value` assignments with secret-looking names are touched, so ordinary
//! logs and hashes survive.

const KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "secret_access_key",
    "secret_key",
    "accountkey",
    "sharedaccesskey",
    "_auth",
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
    // Joined and hyphenated spellings (`SecretAccessKey`, `privateKey`, `--secret-key`).
    "secretaccesskey",
    "secretkey",
    "secret-key",
    "privatekey",
    "private-key",
    "accesskey",
    "passphrase",
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

use std::borrow::Cow;

fn contains_ci(line: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    line.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// A key block that never closes (a source line that merely mentions the header) stops hiding output after this many lines.
const KEY_BLOCK_CAP: usize = 200;

/// Hot path: runs on every captured line, so a clean line allocates nothing.
pub fn redact(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut count = 0usize;
    let mut hidden: Option<usize> = None; // lines swallowed so far inside an open key block
    let mut lines = input.split('\n').peekable();

    while let Some(line) = lines.next() {
        let nl = if lines.peek().is_some() { "\n" } else { "" };
        // `PRIVATE KEY BLOCK-----` is PGP's spelling; one scan rejects the clean lines.
        let pk = contains_ci(line, "PRIVATE KEY")
            && (contains_ci(line, "PRIVATE KEY-----") || contains_ci(line, "PRIVATE KEY BLOCK-----"));
        let has_begin = pk && contains_ci(line, "-----BEGIN");
        let has_end = pk && contains_ci(line, "-----END");

        if let Some(n) = hidden {
            if has_end {
                hidden = None;
                out.push_str(nl);
                continue;
            }
            if n < KEY_BLOCK_CAP {
                hidden = Some(n + 1);
                out.push_str(nl);
                continue;
            }
            hidden = None;
            out.push_str(&format!("[qx] no END line within {KEY_BLOCK_CAP} lines; showing output again\n"));
        }

        if has_begin {
            out.push_str("[REDACTED PRIVATE KEY]");
            count += 1;
            // A JSON-encoded key keeps BEGIN and END on one line; a PEM dump
            // spreads them across lines and must stay suppressed until END. A header that
            // is only quoted in source code does not own the lines after it.
            if !has_end && starts_block(line, lines.peek().copied()) {
                hidden = Some(0);
            }
            out.push_str(nl);
            continue;
        }

        // Bearer first: masking the word "Bearer" as a key value would hide the
        // anchor and leave the actual token exposed.
        let (masked, n1) = mask_auth(line);
        let (masked, n2) = mask_conn_strings(&masked);
        let (masked, n3) = mask_key_values(&masked);
        let (masked, n4) = mask_tokens(&masked);
        count += n1 + n2 + n3 + n4;
        out.push_str(&masked);
        out.push_str(nl);
    }
    (out, count)
}

/// Whether a PEM `BEGIN` line without an `END` opens a multi-line key: the header stands on its
/// own line, or the next line is key material (base64 or a `Proc-Type:` header).
fn starts_block(line: &str, next: Option<&str>) -> bool {
    let t = line.trim();
    (t.starts_with("-----BEGIN") && t.ends_with("-----"))
        || next.is_some_and(|n| {
            let n = n.trim();
            n.starts_with("Proc-Type:")
                || (n.len() >= 16 && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
        })
}

/// `postgres://user:password@host` -> `postgres://user:***@host`.
fn mask_conn_strings(line: &str) -> (Cow<'_, str>, usize) {
    let bytes = line.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut search = 0usize;
    while let Some(pos) = line[search..].find("://") {
        let after = search + pos + 3;
        let mut at = None; // the LAST `@` of the authority: a password may contain `@` or `&`
        let mut k = after;
        while k < bytes.len() {
            let b = bytes[k];
            if b == b'@' {
                at = Some(k);
            } else if b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b'|' | b'/' | b'?' | b'#') {
                // Userinfo ends at the path: `http://localhost:4873/@scope/pkg` has no password.
                break;
            }
            k += 1;
        }
        if let Some(a) = at {
            if let Some(colon) = line[after..a].find(':') {
                let start = after + colon + 1;
                if a > start {
                    ranges.push((start, a));
                }
            }
        }
        search = k.max(after); // resume where the scan stopped: no rescanning a long line per `://`
    }
    mask_ranges(line, ranges)
}

/// Where a key name may begin: a word start, `accessToken` (camelCase) or `PGPASSWORD` (an all-caps name).
/// Not `nopassword`: a lowercase key glued to a lowercase word is some other word.
fn key_starts(b: &[u8], s: usize, e: usize) -> bool {
    let Some(&prev) = s.checked_sub(1).map(|i| &b[i]) else {
        return true;
    };
    !prev.is_ascii_alphanumeric()
        || (prev.is_ascii_lowercase() && b[s].is_ascii_uppercase())
        || (prev.is_ascii_uppercase() && !b[s..e].iter().any(u8::is_ascii_lowercase))
}

/// Looks backwards from each `=`/`:` (or the space in `--password x`) for a secret-looking
/// name, so one pass over the line replaces a scan per key.
fn mask_key_values(line: &str) -> (Cow<'_, str>, usize) {
    let b = line.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let flags = line.contains("--");
    // Linear time on hostile lines: separators inside an already-masked value belong to it, a
    // run of spaces is one flag separator, and an unquoted run is scanned once.
    let mut skip_until = 0usize;
    let mut word = (0usize, 0usize);

    for (p, &sep) in b.iter().enumerate() {
        if p < skip_until {
            continue;
        }
        let flag = sep == b' ' && flags && !(p > 0 && matches!(b[p - 1], b' ' | b'\t'));
        if sep != b'=' && sep != b':' && !flag {
            continue;
        }
        let mut e = p;
        while e > 0 && matches!(b[e - 1], b' ' | b'\t') {
            e -= 1;
        }
        if e > 0 && matches!(b[e - 1], b'"' | b'\'') {
            e -= 1; // the key's closing quote: `"password": "x"`
            if e > 0 && b[e - 1] == b'\\' {
                e -= 1; // ... inside a JSON string: `{\"password\":\"x\"}`
            }
        }
        let key_at = KEYS.iter().find_map(|k| {
            let kb = k.as_bytes();
            (e >= kb.len()
                && b[e - kb.len()..e].eq_ignore_ascii_case(kb)
                && if flag { flag_word_starts(b, e - kb.len()) } else { key_starts(b, e - kb.len(), e) })
            .then_some(e - kb.len())
        });
        let Some(key_at) = key_at else {
            continue;
        };
        let mut j = p + 1;
        while j < b.len() && matches!(b[j], b' ' | b'\t') {
            j += 1;
        }
        if b.get(j) == Some(&b'\\') && b.get(j + 1) == Some(&b'"') {
            // `{\"password\":\"x\"}`: the value runs to the next escaped quote (or the end: fail closed).
            let start = j + 2;
            let end = b[start..].windows(2).position(|w| w == b"\\\"").map_or(b.len(), |i| start + i);
            if end - start >= 4 {
                ranges.push((start, end));
                skip_until = end;
            }
            continue;
        }
        if let Some(&q) = b.get(j).filter(|c| matches!(c, b'"' | b'\'')) {
            // A quoted value is one secret, spaces and `;` included; an unclosed quote runs to the end of
            // the line (fail closed). `password=""` has nothing to mask.
            let start = j + 1;
            let mut k = start;
            while k < b.len() && b[k] != q {
                k += if b[k] == b'\\' { 2 } else { 1 };
            }
            let end = k.min(b.len());
            if end - start >= 4 {
                ranges.push((start, end));
                skip_until = end;
            }
            continue;
        }
        let start = j;
        if word.0 <= start && start < word.1 {
            j = word.1; // inside a run an earlier separator already scanned
        } else {
            while j < b.len() && !matches!(b[j], b' ' | b'\t' | b',' | b';' | b'"' | b'\'' | b'\r' | b'\n') {
                j += 1;
            }
            word = (start, j);
        }
        if j - start >= 4 && !is_code_or_prose(b, key_at, sep, start > p + 1, &b[start..j], &b[j..]) {
            ranges.push((start, j));
            skip_until = j;
        }
    }

    mask_ranges(line, ranges)
}

/// `--auth-token x`: the key ends a word that starts with `--` (not `mytoken x` after some flag).
fn flag_word_starts(b: &[u8], key_at: usize) -> bool {
    let mut w = key_at;
    while w > 0 && (b[w - 1].is_ascii_alphanumeric() || matches!(b[w - 1], b'_' | b'-')) {
        w -= 1;
    }
    key_at >= w + 2 && b[w..].starts_with(b"--")
}

/// An unquoted value that is plainly source or a sentence, not a credential: a path or generic
/// (`std::env::var(`, `Option<String>`), or, after another word and without a digit, a bare type or the
/// start of a sentence (`pub token: String`, `error: invalid secret: expected 32 bytes`). A key that opens
/// a line (`password: Welcome`) is a config entry, so its value is always masked.
fn is_code_or_prose(b: &[u8], key_at: usize, sep: u8, spaced: bool, value: &[u8], rest: &[u8]) -> bool {
    // `::`/`<>` mark code only after `name: ` mid-line or `name = `; a glued `KEY=v` or `--flag v` is config.
    let code_form = match sep {
        b':' => !b[..key_at].trim_ascii_end().is_empty(),
        b'=' => spaced,
        _ => false,
    };
    if code_form && (value.windows(2).any(|w| w == b"::") || value.iter().any(|c| matches!(c, b'<' | b'>'))) {
        return true;
    }
    if sep != b':' || value.iter().any(u8::is_ascii_digit) {
        return false;
    }
    let before = &b[..key_at];
    let after_word = before.last().is_some_and(u8::is_ascii_whitespace)
        && before.trim_ascii_end().last().is_some_and(u8::is_ascii_alphanumeric);
    let camel = value[0].is_ascii_uppercase()
        && value.iter().all(u8::is_ascii_alphabetic)
        && value.iter().any(u8::is_ascii_lowercase);
    let sentence = rest.trim_ascii_start().first().is_some_and(u8::is_ascii_alphanumeric);
    after_word && (camel || sentence)
}

fn mask_ranges(line: &str, mut ranges: Vec<(usize, usize)>) -> (Cow<'_, str>, usize) {
    if ranges.is_empty() {
        return (Cow::Borrowed(line), 0);
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
    let mut out = String::with_capacity(line.len());
    let mut pos = 0usize;
    for (s, e) in &merged {
        out.push_str(&line[pos..*s]);
        out.push_str("***");
        pos = *e;
    }
    out.push_str(&line[pos..]);
    (Cow::Owned(out), merged.len())
}

/// First byte of every entry in `PREFIXES` (a test keeps the two in sync), so
/// most bytes skip the prefix table entirely.
fn is_prefix_start(b: u8) -> bool {
    matches!(b, b's' | b'p' | b'r' | b'g' | b'A' | b'x' | b'y' | b'h' | b'n' | b'd' | b'S' | b'e')
}

fn mask_tokens(line: &str) -> (Cow<'_, str>, usize) {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut copied = 0usize;
    let mut count = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if is_prefix_start(bytes[i]) {
            // `risk-assessment` is not an `sk-` key: a token starts at a word edge.
            let edge = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || matches!(bytes[i - 1], b'_' | b'-'));
            // The table scan is the costly part: run it only at a word edge.
            if let Some((plen, min)) = if edge { prefix_at(bytes, i) } else { None } {
                let mut j = i + plen;
                while j < bytes.len() && is_token_byte(bytes[j]) {
                    j += 1;
                }
                // Real npm tokens are plain alphanumerics; `npm_package_name` is an env var.
                let plain = &bytes[i..i + plen] != b"npm_" || bytes[i + plen..j].iter().all(u8::is_ascii_alphanumeric);
                if plain && j - (i + plen) >= min {
                    out.push_str(&line[copied..i + plen]);
                    out.push_str("***");
                    count += 1;
                    copied = j;
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    if count == 0 {
        return (Cow::Borrowed(line), 0);
    }
    out.push_str(&line[copied..]);
    (Cow::Owned(out), count)
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

/// `Bearer <token>` anywhere in the line, and `Basic <base64>` when the line is an Authorization header.
fn mask_auth(line: &str) -> (Cow<'_, str>, usize) {
    let bytes = line.as_bytes();
    let basic = contains_ci(line, "authorization");
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (scheme, min) in [("bearer ", 12), ("basic ", 8)] {
        if scheme == "basic " && !basic {
            continue;
        }
        let n = scheme.len();
        for pos in (0..bytes.len().saturating_sub(n - 1)).filter(|&p| bytes[p..p + n].eq_ignore_ascii_case(scheme.as_bytes())) {
            let start = pos + n;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_graphic() && !matches!(bytes[end], b'"' | b'\'') {
                end += 1;
            }
            if end - start >= min {
                ranges.push((start, end));
            }
        }
    }
    mask_ranges(line, ranges)
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

    #[test]
    fn prefix_start_table_matches_prefixes() {
        for (p, _) in PREFIXES {
            assert!(is_prefix_start(p.as_bytes()[0]), "{p}");
        }
    }

    #[test]
    fn key_value_forms() {
        let (s, n) = redact("PASSWORD = hunter2xyz and client_secret:\"abcd1234\" x-token=abc key: value\nnopassword=abcdefgh\n");
        assert_eq!(n, 2, "{s}");
        assert!(s.contains("PASSWORD = ***") && s.contains("client_secret:\"***\""));
        assert!(s.contains("nopassword=abcdefgh")); // key must start at a word boundary
        assert!(s.contains("x-token=abc")); // value under 4 bytes is not a secret
    }

    #[test]
    fn masks_json_and_compound_secret_keys() {
        for (line, secret) in [
            (r#"{"password": "hunter2hunter2", "user": "bob"}"#, "hunter2hunter2"),
            (r#"{"api_key":"abcdef0123456789"}"#, "abcdef0123456789"),
            ("PGPASSWORD=hunter2xyz psql", "hunter2xyz"),
            ("DJANGO_SECRET_KEY=abcd1234wxyz", "abcd1234wxyz"),
            (r#"{"accessToken":"abcd1234wxyz"}"#, "abcd1234wxyz"),
            ("//registry.npmjs.org/:_authToken=abcd1234wxyz", "abcd1234wxyz"),
            ("//registry.npmjs.org/:_auth=abcd1234wxyz", "abcd1234wxyz"),
            ("Authorization: Basic dXNlcjpwYXNzd29yZA==", "dXNlcjpwYXNzd29yZA=="),
            ("DefaultEndpointsProtocol=https;AccountKey=abcd1234wxyz==;", "abcd1234wxyz"),
            ("Endpoint=sb://x;SharedAccessKey=abcd1234wxyz", "abcd1234wxyz"),
            ("mytool --password hunter2xyz --verbose", "hunter2xyz"),
        ] {
            let (s, n) = redact(line);
            assert!(n >= 1 && !s.contains(secret), "{line} -> {s}");
        }
        assert_eq!(redact("Basic usage of the tool: see docs").0, "Basic usage of the tool: see docs");
        assert!(redact("nopassword=abcdefgh").0.contains("abcdefgh"));
    }

    #[test]
    fn paths_branches_and_env_names_are_not_secrets() {
        for ok in [
            "On branch feature/risk-assessment-engine-refactor",
            "src/disk-usage-monitor-service.ts",
            "npm_package_dependencies_left_pad_version=1.3.0",
            "PWD=/c/Users/x/project",
            "fetching http://localhost:4873/@scope/pkg/-/pkg-1.0.0.tgz",
        ] {
            assert_eq!(redact(ok), (ok.to_string(), 0), "{ok}");
        }
        assert!(!redact("export NPM=npm_abcdefghijklmnopqrstuvwxyz0123456789").0.contains("abcdefghijklmnop"));
        assert!(!redact("url sk-abcdefghijklmnopqrstuvwxyz0123").0.contains("abcdefghijklmnop"));
    }

    #[test]
    fn a_key_header_in_source_does_not_hide_the_rest() {
        let (s, _) = redact("let hdr = \"BEGIN PRIVATE KEY\";\nnext line\n");
        assert!(s.contains("next line"));
        let open = format!("-----BEGIN PRIVATE KEY-----\n{}tail-marker\n", "AAAA\n".repeat(300));
        let (s, n) = redact(&open);
        assert_eq!(n, 1);
        assert!(s.matches("AAAA").count() == 100 && s.contains("no END line") && s.contains("tail-marker"), "{s}");
    }

    #[test]
    fn pgp_blocks_and_quoted_values_are_masked_whole() {
        let pgp = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nlQdGBGabcdef\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        let (s, n) = redact(pgp);
        assert!(n == 1 && !s.contains("lQdGBG") && s.contains("after"), "{s}");
        for (line, secret) in [
            (r#"password="correct horse; battery""#, "horse"),
            (r#"password = "correct horse battery staple""#, "staple"),
            ("secret='my long passphrase here'", "passphrase"),
            (r#"mytool --password "correct horse" --v"#, "horse"),
            (r#"{"token": "ab cd\" ef gh"}"#, "ef gh"),
            ("password=\"unclosed secret words", "words"),
        ] {
            let (s, n) = redact(line);
            assert!(n >= 1 && !s.contains(secret), "{line} -> {s}");
        }
        assert_eq!(redact(r#"password="" next=abcdefgh"#).1, 0); // an empty quoted value masks nothing
    }

    #[test]
    fn source_code_and_error_text_after_a_key_name_survive() {
        for ok in [
            "pub token: String,",
            "pub password: Option<String>,",
            "let secret = std::env::var(\"SECRET_KEY\")?;",
            "error: invalid secret: expected 32 bytes",
            "Unexpected token: ExpiredSignature",
        ] {
            assert_eq!(redact(ok), (ok.to_string(), 0), "{ok}");
        }
        // a key that opens the line, or a value with a digit, is still a credential
        // glued `KEY=value` and `--flag value` are config even when the value holds `::` or `<>`
        for line in [
            "password: Welcome",
            "  token: abcdefgh",
            "db password: hunter2xyz",
            "api secret=Welcome",
            "password=a>b1234",
            "DB_PASSWORD=Ab12<Cd34xyz",
            "token=a::b1234",
            "api_key=abcd>efghijkl",
            "x --password a>b1234",
            "password: a>b1234",
        ] {
            let (s, n) = redact(line);
            assert!(n == 1 && s.contains("***"), "{line} -> {s}");
        }
    }

    #[test]
    fn every_bearer_is_masked_and_long_urls_are_linear() {
        let (s, n) = redact("a Bearer aaaaaaaaaaaaaaaa b Bearer bbbbbbbbbbbbbbbb");
        assert!(n == 2 && !s.contains("aaaaaaaa") && !s.contains("bbbbbbbb"), "{s}");
        let long = "://".repeat(100_000);
        let t = std::time::Instant::now();
        let _ = redact(&long);
        assert!(t.elapsed().as_secs() < 2, "quadratic scan: {:?}", t.elapsed());
    }

    #[test]
    fn masks_joined_secret_names() {
        for (line, secret) in [
            (r#"{"SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"}"#, "wJalrXUtnFEMI"),
            ("secretKey: 'abcd1234wxyz'", "abcd1234wxyz"),
            ("privateKey=abcd1234wxyz", "abcd1234wxyz"),
            ("accessKey=abcd1234wxyz", "abcd1234wxyz"),
            ("PASSPHRASE=hunter2xyz", "hunter2xyz"),
        ] {
            let (s, n) = redact(line);
            assert!(n >= 1 && !s.contains(secret), "{line} -> {s}");
        }
    }

    #[test]
    fn masks_secrets_inside_escaped_json_strings() {
        let line = r#"{\"password\":\"hunter2hunter2\",\"user\":\"bob\"}"#;
        let (s, n) = redact(line);
        assert!(n >= 1 && !s.contains("hunter2hunter2") && s.contains("bob"), "{s}");
    }

    #[test]
    fn masks_flags_whose_name_ends_in_a_secret_word() {
        for (line, secret) in [
            ("mytool --auth-token abcd1234wxyz", "abcd1234wxyz"),
            ("mysql --db-password hunter2xyz --verbose", "hunter2xyz"),
        ] {
            let (s, n) = redact(line);
            assert!(n >= 1 && !s.contains(secret), "{line} -> {s}");
        }
        // a word that merely ends in a key name is not a flag
        assert_eq!(redact("run --x mytoken abcd1234wxyz").1, 0);
    }

    #[test]
    fn repeated_keys_and_space_runs_are_linear() {
        let t = std::time::Instant::now();
        let _ = redact(&"token=".repeat(40_000));
        let _ = redact(&format!("--{}x", " ".repeat(100_000)));
        assert!(t.elapsed().as_secs() < 1, "quadratic scan: {:?}", t.elapsed());
    }

    #[test]
    fn masks_url_passwords_that_contain_at_or_ampersand() {
        for (line, secret) in [
            ("mongodb://admin:p@ssw0rdSecret@db.example/app", "ssw0rdSecret"),
            ("postgres://u:pa&ss1234@h/db", "pa&ss1234"),
        ] {
            let (s, n) = redact(line);
            assert!(n >= 1 && !s.contains(secret), "{line} -> {s}");
        }
        // no password: nothing to mask
        assert_eq!(redact("ssh://git@github.com:22/org/repo").1, 0);
    }

    #[test]
    fn a_pem_header_in_source_does_not_swallow_the_lines_after_it() {
        let input = "let hdr = \"-----BEGIN PRIVATE KEY-----\";\nerror[E0308]: mismatched types\n  --> src/a.rs:1:1\n";
        let (s, _) = redact(input);
        assert!(s.contains("error[E0308]: mismatched types") && s.contains("--> src/a.rs:1:1"), "{s}");
        // a real block is still masked whole
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\nafter\n";
        let (s, n) = redact(pem);
        assert!(n == 1 && !s.contains("MIIEvQ") && s.contains("after"), "{s}");
    }
}
