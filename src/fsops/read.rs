//! The `read`, `list` and `stat` operations: rendering, decoding and read memory.

use super::*;

/// What `read` already sent for one file: which lines, of which content. A repeat read of lines the
/// agent already holds, from a file that has not changed, answers with one line instead of the text.
struct Served {
    version: Version,
    lo: usize,
    hi: usize,
    max_line_chars: usize,
    at: std::time::Instant,
}

const SERVED_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const SERVED_MAX: usize = 256;

fn served() -> &'static Mutex<HashMap<PathBuf, Served>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, Served>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `QX_READ_MEMORY=off` turns the stubs off for the whole server.
fn read_memory_on() -> bool {
    std::env::var("QX_READ_MEMORY").map(|v| !v.eq_ignore_ascii_case("off")).unwrap_or(true)
}

/// True when lines `lo..=hi` of this exact content were already sent, at least as wide, within the idle limit.
fn already_served(path: &Path, version: Version, lo: usize, hi: usize, max_line_chars: usize) -> bool {
    let map = served().lock().unwrap_or_else(|e| e.into_inner());
    map.get(path).is_some_and(|s| {
        s.version.len == version.len
            && s.version.hash == version.hash
            && s.at.elapsed() < SERVED_IDLE
            && s.lo <= lo
            && hi <= s.hi
            && s.max_line_chars >= max_line_chars
    })
}

/// Note that lines `lo..=hi` reached the agent. Adjacent or overlapping windows of the same content merge.
fn note_served(path: &Path, version: Version, lo: usize, hi: usize, max_line_chars: usize) {
    if hi < lo {
        return;
    }
    let mut map = served().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() >= SERVED_MAX && !map.contains_key(path) {
        map.retain(|_, s| s.at.elapsed() < SERVED_IDLE);
        if map.len() >= SERVED_MAX {
            map.clear();
        }
    }
    let same = |s: &Served| {
        s.version.len == version.len
            && s.version.hash == version.hash
            && s.at.elapsed() < SERVED_IDLE
            && s.max_line_chars == max_line_chars
            && lo <= s.hi + 1
            && s.lo <= hi + 1
    };
    let (lo, hi) = match map.get(path) {
        Some(s) if same(s) => (lo.min(s.lo), hi.max(s.hi)),
        _ => (lo, hi),
    };
    map.insert(path.to_path_buf(), Served { version, lo, hi, max_line_chars, at: std::time::Instant::now() });
}

// --------------------------------------------------------------------- read

pub(super) fn read(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let offset = usize_arg(args, "offset", 1, 1, usize::MAX / 2)?;
    let limit = usize_arg(args, "limit", DEFAULT_READ_LINES, 1, MAX_READ_LINES)?;
    let max_line_chars = usize_arg(
        args,
        "max_line_chars",
        DEFAULT_READ_LINE_CHARS,
        80,
        MAX_READ_LINE_CHARS,
    )?;
    let path = root.resolve_read(raw).map_err(|e| e.fb("read"))?;
    let md = fs::metadata(&path).ok();
    if md.as_ref().is_some_and(|m| m.is_file() && m.len() > MAX_READ_BYTES) {
        return read_large(root, &path, raw, offset, limit, max_line_chars);
    }
    if md.as_ref().is_some_and(|m| m.is_dir()) {
        // A directory is a listing: the same answer `list` gives, in one call instead of an error and a retry.
        return list(root, &json!({ "path": raw }));
    }
    let (bytes, version) = read_file(&path, "read")?;
    if bytes
        .iter()
        .take(8192)
        .any(|&b| b == 0 && !(bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])))
    {
        return Err(FsError::new(
            "FS_BINARY",
            format!("cannot read \"{raw}\" as text: binary data"),
        )
        .fb("read"));
    }
    let text = decode_text(&bytes);
    let display = root.display(&path);
    // Memory of what this server already sent: an unchanged file and a window the agent holds get a stub.
    let total = count_lines(text.as_bytes());
    let want_hi = offset.saturating_add(limit - 1).min(total);
    if read_memory_on() && !bool_arg(args, "fresh", false) && total > 0 && offset <= total
        && already_served(&path, version, offset, want_hi, max_line_chars)
    {
        remember(&path, version);
        return Ok(Reply {
            text: format!(
                "[qx] unchanged since your last read (lines {offset}-{want_hi} of {total}); fresh=true sends it again"
            ),
            structured: json!({
                "op": "read",
                "path": display,
                "lines": 0,
                "total_lines": total,
                "end_line": want_hi,
                "truncated": false,
                "unchanged": true,
                "start_line": offset,
                "bytes": bytes.len(),
            }),
            is_error: false,
        });
    }
    let out = render_read(
        &display,
        &text,
        offset,
        limit,
        max_line_chars,
        DEFAULT_READ_BYTES,
    )?;
    remember(&path, version);
    if out.lines > 0 {
        note_served(&path, version, offset, out.end_line, max_line_chars);
    }
    Ok(Reply {
        text: out.text,
        structured: json!({
            "op": "read",
            "path": display,
            "lines": out.lines,
            "total_lines": out.total,
            "end_line": out.end_line,
            "truncated": out.truncated,
            "bytes": bytes.len(),
        }),
        is_error: false,
    })
}

/// A file too big to load: stream it to the requested window. Not observed, so it cannot be
/// written back whole; `edit` still works on files under its own size cap.
fn read_large(root: &Root, path: &Path, raw: &str, offset: usize, limit: usize, max_line_chars: usize) -> Result<Reply, FsError> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(path).map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::with_capacity(1 << 20, f);
    let head = r.fill_buf().map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
    if head.iter().take(8192).any(|&b| b == 0) {
        return Err(FsError::new("FS_BINARY", format!("cannot read \"{raw}\" as text: binary data")).fb("read"));
    }
    let (mut buf, mut body) = (Vec::new(), String::new());
    let (mut n, mut shown, mut more) = (0usize, 0usize, false);
    let mut end_line = offset.saturating_sub(1);
    loop {
        buf.clear();
        let got = r.read_until(b'\n', &mut buf).map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
        if got == 0 {
            break;
        }
        n += 1;
        if n < offset {
            continue;
        }
        if shown >= limit {
            more = true;
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        let mut one = String::new();
        push_usize(&mut one, n);
        one.push_str(": ");
        push_clipped(&mut one, line, max_line_chars);
        if body.len() + one.len() + 1 > DEFAULT_READ_BYTES {
            more = true;
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&one);
        shown += 1;
        end_line = n;
    }
    let display = root.display(path);
    let text = if lean() {
        lean_read(&body, offset, end_line, (!more).then_some(n), more)
    } else {
        let footer = if more {
            format!("(Showing lines {offset}-{end_line} of a {size}-byte file. Use offset={} to continue.)", end_line + 1)
        } else {
            format!("(End of file - total {n} lines)")
        };
        format!("<path>{display}</path>\n<type>file</type>\n<content>\n{body}\n\n{footer}\n</content>")
    };
    Ok(Reply {
        text,
        structured: json!({ "op": "read", "path": display, "lines": shown, "end_line": end_line, "truncated": more, "bytes": size }),
        is_error: false,
    })
}

/// Claude Code and Codex get a read without the tag frame (the call already names the path, and a
/// batch header repeats it): the numbered lines, plus one bracketed line only when the window is
/// not the whole file from line 1. Other clients keep the OpenCode-shaped frame.
pub(super) fn lean() -> bool {
    matches!(crate::client::current(), crate::client::Client::ClaudeCode | crate::client::Client::Codex)
}

fn lean_read(body: &str, offset: usize, end: usize, total: Option<usize>, more: bool) -> String {
    let note = if more {
        let of = total.map(|t| format!(" of {t}")).unwrap_or_default();
        format!("[lines {offset}-{end}{of}; offset={} for more]", end + 1)
    } else if total == Some(0) {
        "(empty file)".to_string()
    } else if offset > 1 {
        format!("[end, {} lines]", total.unwrap_or(end))
    } else {
        return body.to_string();
    };
    if body.is_empty() {
        note
    } else {
        format!("{body}\n{note}")
    }
}

struct ReadWindow {
    text: String,
    lines: usize,
    total: usize,
    end_line: usize,
    truncated: bool,
}

fn render_read(
    display: &str,
    text: &str,
    offset: usize,
    limit: usize,
    max_line_chars: usize,
    max_bytes: usize,
) -> Result<ReadWindow, FsError> {
    let total = count_lines(text.as_bytes());
    if offset > total && !(total == 0 && offset == 1) {
        let note = format!("(offset {offset} is past the end: the file has {total} lines)");
        return Ok(ReadWindow {
            text: if lean() { note } else { format!("<path>{display}</path>\n<type>file</type>\n<content>\n{note}\n</content>") },
            lines: 0,
            total,
            end_line: total,
            truncated: false,
        });
    }

    let mut body = String::with_capacity(max_bytes.min(text.len() + 128));
    let mut linebuf = String::with_capacity(256);
    let mut shown = 0usize;
    let mut end_line = offset.saturating_sub(1);
    let mut cut_by_bytes = false;

    for (idx, line) in text.split('\n').enumerate() {
        let n = idx + 1;
        if n > total {
            break;
        }
        if n < offset {
            continue;
        }
        if shown >= limit {
            break;
        }
        linebuf.clear();
        push_usize(&mut linebuf, n);
        linebuf.push_str(": ");
        push_clipped(
            &mut linebuf,
            line.strip_suffix('\r').unwrap_or(line),
            max_line_chars,
        );
        let add = linebuf.len() + usize::from(!body.is_empty());
        if body.len() + add > max_bytes {
            cut_by_bytes = true;
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&linebuf);
        shown += 1;
        end_line = n;
    }

    let text = if lean() {
        lean_read(&body, offset, end_line, Some(total), cut_by_bytes || end_line < total)
    } else {
        let footer = if cut_by_bytes {
            format!(
                "(Output capped. Showing lines {offset}-{end_line}. Use offset={} to continue.)",
                end_line + 1
            )
        } else if end_line < total {
            format!(
                "(Showing lines {offset}-{end_line} of {total}. Use offset={} to continue.)",
                end_line + 1
            )
        } else {
            format!("(End of file - total {total} lines)")
        };
        let body = if body.is_empty() {
            footer
        } else {
            format!("{body}\n\n{footer}")
        };
        format!("<path>{display}</path>\n<type>file</type>\n<content>\n{body}\n</content>")
    };
    Ok(ReadWindow {
        text,
        lines: shown,
        total,
        end_line,
        truncated: cut_by_bytes || end_line < total,
    })
}

/// Lines in `text`: a last one without its `\n` counts too.
pub(super) fn count_lines(text: &[u8]) -> usize {
    text.iter().filter(|&&b| b == b'\n').count() + usize::from(text.last().is_some_and(|&b| b != b'\n'))
}

pub(super) fn decode_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return std::borrow::Cow::Owned(crate::exec::decode(bytes));
    }
    let s = String::from_utf8_lossy(bytes);
    // A UTF-8 BOM is not part of the first line; write keeps it (see `carry_over_encoding`).
    if let Some(rest) = s.strip_prefix('\u{feff}') {
        return std::borrow::Cow::Owned(rest.replace('\0', ""));
    }
    if s.contains('\0') {
        std::borrow::Cow::Owned(s.replace('\0', ""))
    } else {
        s
    }
}

pub(super) fn push_usize(out: &mut String, mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap_or("0"));
}

pub(super) fn push_clipped(out: &mut String, line: &str, max: usize) {
    let end = line.char_indices().nth(max).map_or(line.len(), |(idx, _)| idx);
    out.push_str(&line[..end]);
    if end < line.len() {
        out.push_str("... (line truncated to ");
        push_usize(out, max);
        out.push_str(" chars)");
    }
}

/// A grep hit shown within `max` characters: a long line is cut around the first match, not at its head,
/// so the text that matched is in what the model sees.
pub(super) fn clip_match(line: &str, max: usize, matcher: &Matcher) -> String {
    let mut out = String::with_capacity(line.len().min(max + 64));
    let before = matcher.find(line).map_or(0, |m| line[..m.start()].chars().count());
    let start = before.saturating_sub(max / 2);
    if start == 0 {
        push_clipped(&mut out, line, max);
        return out;
    }
    let from = line.char_indices().nth(start).map_or(line.len(), |(i, _)| i);
    out.push_str("...");
    push_clipped(&mut out, &line[from..], max);
    out
}

// --------------------------------------------------------------------- list

pub(super) fn list(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = path_arg(args).unwrap_or(".");
    let path = root.resolve_read(raw).map_err(|e| e.fb("list"))?;
    let md = fs::metadata(&path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw}\""), e).fb("list"))?;
    let mut rows: Vec<(u8, String, u64, String)> = Vec::new();
    if md.is_dir() {
        let entries = fs::read_dir(&path)
            .map_err(|e| FsError::io(format!("cannot list \"{raw}\""), e).fb("list"))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let (kind, size, extra) = if ft.is_symlink() {
                let target = fs::read_link(entry.path())
                    .map(|p| format!(" -> {}", p.display()))
                    .unwrap_or_default();
                (2u8, 0u64, target)
            } else if ft.is_dir() {
                (0, 0, String::new())
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                (1, size, String::new())
            };
            rows.push((kind, name, size, extra));
        }
    } else {
        rows.push((
            1,
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| raw.to_string()),
            md.len(),
            String::new(),
        ));
    }
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    let shown = rows.len().min(MAX_LIST_ENTRIES);
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("(empty)\n");
    }
    for (kind, name, size, extra) in rows.iter().take(shown) {
        match kind {
            0 => out.push_str(&format!("d {name}/\n")),
            2 => out.push_str(&format!("l {name}{extra}\n")),
            _ => out.push_str(&format!("f {size} {name}\n")),
        }
    }
    if rows.len() > shown {
        out.push_str(&format!(
            "[qx] showing {shown} of {} entries; use the native {} tool for the rest.\n",
            rows.len(),
            native(Native::Glob)
        ));
    }
    Ok(Reply {
        text: out,
        structured: json!({
            "op": "list",
            "path": root.display(&path),
            "count": rows.len(),
            "shown": shown,
            "truncated": rows.len() > shown,
        }),
        is_error: false,
    })
}

pub(super) fn stat(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let path = root.resolve_read(raw).map_err(|e| e.fb("stat"))?;
    let md = fs::metadata(&path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw}\""), e).fb("stat"))?;
    let kind = if md.is_dir() {
        "dir"
    } else if md.is_file() {
        "file"
    } else {
        "other"
    };
    let modified = md
        .modified()
        .map(crate::builtin::format_time)
        .unwrap_or_else(|_| "unknown".to_string());
    let text = format!(
        "path: {}\ntype: {kind}\nsize: {}\nmodified: {modified}\nreadonly: {}",
        root.display(&path),
        md.len(),
        md.permissions().readonly()
    );
    Ok(Reply {
        text,
        structured: json!({
            "op": "stat",
            "path": root.display(&path),
            "kind": kind,
            "size": md.len(),
            "readonly": md.permissions().readonly(),
        }),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeat_read_of_an_unchanged_file_is_a_stub_until_the_file_changes() {
        let dir = tmp("read-memory");
        let f = dir.join("a.txt");
        fs::write(&f, "one\ntwo\nthree\nfour\n").unwrap();
        let first = invoke(&dir, "read", json!({"path": "a.txt"}));
        assert!(first.text.contains("four"), "{}", first.text);
        // Same window again: one line, no content.
        let again = invoke(&dir, "read", json!({"path": "a.txt"}));
        assert!(again.text.starts_with("[qx] unchanged since your last read"), "{}", again.text);
        assert!(!again.text.contains("four"));
        assert_eq!(again.structured["unchanged"], json!(true));
        // A window inside what was sent is covered too.
        let inner = invoke(&dir, "read", json!({"path": "a.txt", "offset": 2, "limit": 2}));
        assert!(inner.text.starts_with("[qx] unchanged"), "{}", inner.text);
        // fresh=true always sends the text.
        let fresh = invoke(&dir, "read", json!({"path": "a.txt", "fresh": true}));
        assert!(fresh.text.contains("four"), "{}", fresh.text);
        // A change of content (same size) is sent in full again.
        fs::write(&f, "one\ntwo\nthree\nFOUR\n").unwrap();
        let changed = invoke(&dir, "read", json!({"path": "a.txt"}));
        assert!(changed.text.contains("FOUR"), "{}", changed.text);
    }

    #[test]
    fn read_memory_never_stubs_lines_that_were_not_sent() {
        let dir = tmp("read-memory-range");
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        fs::write(dir.join("b.txt"), body).unwrap();
        let head = invoke(&dir, "read", json!({"path": "b.txt", "limit": 3}));
        assert!(head.text.contains("line3") && !head.text.contains("line4"), "{}", head.text);
        // The rest of the file was never sent: it must come through, and then everything is covered.
        let rest = invoke(&dir, "read", json!({"path": "b.txt", "offset": 4}));
        assert!(rest.text.contains("line10"), "{}", rest.text);
        let all = invoke(&dir, "read", json!({"path": "b.txt"}));
        assert!(all.text.starts_with("[qx] unchanged"), "{}", all.text);
    }

    #[test]
    fn read_numbers_lines_and_observes_the_file() {
        // Checked in the framed shape: pin a client that gets the frame (the process-wide client is
        // changed by other tests while this one runs).
        crate::client::scoped(crate::client::Client::Generic, read_numbers_lines_body);
    }
    fn read_numbers_lines_body() {
        let dir = tmp("read");
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let r = invoke(&dir, "read", json!({"path":"a.txt"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("1: one"));
        assert!(r.text.contains("3: three"));
        assert!(r.text.contains("End of file - total 3 lines"));
        assert_eq!(r.structured["total_lines"], 3);

        let r = invoke(&dir, "write", json!({"path":"a.txt","content":"changed\n"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("Updated file (+1 -3 lines)"), "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "changed\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_and_codex_read_replies_drop_the_frame_but_keep_every_signal() {
        use crate::client::{scoped, Client};
        let dir = tmp("lean-read");
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(dir.join("empty.txt"), "").unwrap();
        for c in [Client::ClaudeCode, Client::Codex] {
            // `fresh`: this test checks the framing of a reply, not the read memory.
            let t = |mut args: Value| {
                args["fresh"] = json!(true);
                scoped(c, || invoke(&dir, "read", args)).text
            };
            // Whole file from line 1: the lines and nothing else.
            assert_eq!(t(json!({"path": "a.txt"})), "1: one\n2: two\n3: three");
            // A window says where it is and how to go on; the end of a later window says it is the end.
            assert_eq!(t(json!({"path": "a.txt", "limit": 1})), "1: one\n[lines 1-1 of 3; offset=2 for more]");
            assert_eq!(t(json!({"path": "a.txt", "offset": 2})), "2: two\n3: three\n[end, 3 lines]");
            assert_eq!(t(json!({"path": "a.txt", "offset": 9})), "(offset 9 is past the end: the file has 3 lines)");
            assert_eq!(t(json!({"path": "empty.txt"})), "(empty file)");
            assert_eq!(
                scoped(c, || invoke(&dir, "write", json!({"path": "w.txt", "content": "x\n"}))).text,
                "Created file (+1 -0 lines)"
            );
            fs::remove_file(dir.join("w.txt")).unwrap();
            // A batch keeps one header per item and no tags inside it.
            let b = scoped(c, || invoke(&dir, "read", json!({"paths": ["a.txt", "empty.txt"], "fresh": true}))).text;
            assert_eq!(b, "== 1/2 read a.txt ==\n1: one\n2: two\n3: three\n\n== 2/2 read empty.txt ==\n(empty file)");
        }
        // Everyone else keeps the tag frame.
        let r = scoped(Client::OpenCode, || invoke(&dir, "read", json!({"path": "a.txt", "fresh": true})));
        assert!(r.text.starts_with("<path>a.txt</path>\n<type>file</type>\n<content>\n1: one"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_stat_work() {
        let dir = tmp("list");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "abc").unwrap();
        let r = invoke(&dir, "list", json!({}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("d src/"));
        let r = invoke(&dir, "stat", json!({"path":"src/a.rs"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("type: file"));
        assert!(r.text.contains("size: 3"));
        let _ = fs::remove_dir_all(&dir);
    }

}
