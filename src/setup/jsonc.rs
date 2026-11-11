//! Byte-preserving JSONC editing for the CLI configs that allow comments.
//!
//! Rewriting a config with `serde_json` would reorder keys, drop comments and drop
//! trailing commas — and these files are hand-edited. Every function here edits the
//! text in place: bytes outside the member being changed are copied verbatim.

use serde_json::Value;
use std::ops::Range;

/// The byte range of the value at `path`, or `None` when it is absent.
pub fn member(text: &str, path: &[&str]) -> Result<Option<Range<usize>>, String> {
    // A blank file is an empty document: nothing in it.
    let Ok(mut start) = first_value(text) else { return Ok(None) };
    for (depth, key) in path.iter().enumerate() {
        let (list, _) = members(text, start)?;
        let Some(hit) = list.iter().find(|m| m.key == *key) else {
            return Ok(None);
        };
        if depth + 1 == path.len() {
            return Ok(Some(hit.value_start..hit.value_end));
        }
        let bytes = text.as_bytes();
        if bytes.get(hit.value_start) != Some(&b'{') {
            // A non-object on the way down means the member we want cannot exist.
            return Ok(None);
        }
        start = hit.value_start;
    }
    // An empty path means the whole document's value.
    let mut end = start;
    value(text.as_bytes(), &mut end)?;
    Ok(Some(start..end))
}

/// Replace the value at `path`, or insert the member (creating parent objects).
pub fn set_member(text: &str, path: &[&str], value: &Value) -> Result<String, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if let Some(base) = empty_baseline(text) {
        return set_member(&base, path, value);
    }
    if let Some(range) = member(text, path)? {
        let current = text[range.clone()].trim();
        if current == serde_json::to_string(value).unwrap_or_default().as_str() {
            // Same bytes: nothing to do, so repeated runs stay Unchanged.
            return Ok(text.to_string());
        }
        let indent = line_indent(text, range.start);
        let mut out = String::with_capacity(text.len() + 64);
        out.push_str(&text[..range.start]);
        out.push_str(&reindent(value, &indent, eol(text)));
        out.push_str(&text[range.end..]);
        return Ok(out);
    }
    insert_member(text, path, value)
}

/// Delete the member at `path` (a value replaced by `Removed` when it is absent).
pub fn remove_member(text: &str, path: &[&str]) -> Result<String, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if first_value(text).is_err() {
        return Ok(text.to_string());
    }
    // An absent (or non-object) parent holds nothing to remove: that is a no-op, not an error.
    if path.len() > 1 && member(text, &path[..path.len() - 1])?.is_none() {
        return Ok(text.to_string());
    }
    let parent_start = parent_object(text, path)?;
    let (list, _close) = members(text, parent_start)?;
    let key = path[path.len() - 1];
    let Some(index) = list.iter().position(|m| m.key == key) else {
        return Ok(text.to_string());
    };
    Ok(cut(text, parent_start, list[index].key_start, list[index].value_end))
}

/// Remove the empty object or array at `path`, then each parent that is left empty, up to
/// the document, which becomes `{}`. Anything inside, even a comment, stops it.
pub fn prune_empty(text: &str, path: &[&str]) -> Result<String, String> {
    let mut text = text.to_string();
    let mut path = path;
    while !path.is_empty() {
        let Some(r) = member(&text, path)? else { return Ok(text) };
        if !is_empty_container(&text, &r) {
            return Ok(text);
        }
        text = remove_member(&text, path)?;
        path = &path[..path.len() - 1];
    }
    if let Some(r) = member(&text, &[])? {
        if text[r.clone()].starts_with('{') && is_empty_container(&text, &r) {
            text.replace_range(r, "{}");
        }
    }
    Ok(text)
}

fn is_empty_container(text: &str, r: &Range<usize>) -> bool {
    let s = &text[r.clone()];
    (s.starts_with('{') || s.starts_with('[')) && text[r.start + 1..r.end - 1].trim().is_empty()
}

/// An array's `[` and `]` offsets, and the byte range of each element.
type Array = (Range<usize>, Vec<Range<usize>>);

/// The array at `path`, or `None` when the member is absent.
fn array(text: &str, path: &[&str]) -> Result<Option<Array>, String> {
    let Some(range) = member(text, path)? else { return Ok(None) };
    let b = text.as_bytes();
    if b.get(range.start) != Some(&b'[') {
        return Err(format!("{}: not an array", path.join(".")));
    }
    let mut i = range.start + 1;
    let mut items = Vec::new();
    loop {
        skip(b, &mut i);
        if b.get(i) == Some(&b']') {
            return Ok(Some((range.start..i, items)));
        }
        let start = i;
        value(b, &mut i)?;
        items.push(start..i);
        skip(b, &mut i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b']') => return Ok(Some((range.start..i, items))),
            _ => return Err("expected ',' or ']'".into()),
        }
    }
}

/// The elements of the array at `path`, parsed; empty when it is absent.
pub fn elements(text: &str, path: &[&str]) -> Result<Vec<Value>, String> {
    let Some((_, items)) = array(text, path)? else { return Ok(Vec::new()) };
    items
        .into_iter()
        .map(|r| serde_json::from_str(&to_strict_json(&text[r])).map_err(|e| e.to_string()))
        .collect()
}

/// Append `value` to the array at `path`, creating the array when absent.
pub fn push_element(text: &str, path: &[&str], value: &Value) -> Result<String, String> {
    let Some((brackets, items)) = array(text, path)? else {
        return set_member(text, path, &Value::Array(vec![value.clone()]));
    };
    let nl = eol(text);
    let close_indent = line_indent(text, brackets.end);
    let indent = items
        .iter()
        .map(|r| line_indent(text, r.start))
        .find(|indent| indent.len() > close_indent.len())
        .unwrap_or_else(|| format!("{close_indent}  "));
    let mut out = String::with_capacity(text.len() + 256);
    match items.last() {
        // After the last element, so a trailing comma or comment stays where it was.
        Some(last) => {
            out.push_str(&text[..last.end]);
            out.push_str(&format!(",{nl}{indent}{}", reindent(value, &indent, nl)));
            out.push_str(&text[last.end..]);
        }
        None => {
            out.push_str(&text[..brackets.start + 1]);
            out.push_str(&format!("{nl}{indent}{}{nl}{close_indent}", reindent(value, &indent, nl)));
            out.push_str(&text[brackets.end..]);
        }
    }
    Ok(out)
}

/// Swap element `index` of the array at `path` for `value`, leaving every other byte alone.
pub fn replace_element(text: &str, path: &[&str], index: usize, value: &Value) -> Result<String, String> {
    let (_, items) = array(text, path)?.ok_or_else(|| format!("{}: no such array", path.join(".")))?;
    let r = items
        .get(index)
        .ok_or_else(|| format!("{}: no element {index}", path.join(".")))?;
    let indent = line_indent(text, r.start);
    Ok(format!("{}{}{}", &text[..r.start], reindent(value, &indent, eol(text)), &text[r.end..]))
}

/// Remove every element of the array at `path` that `drop` picks.
pub fn remove_elements(text: &str, path: &[&str], drop: impl Fn(&Value) -> bool) -> Result<String, String> {
    let Some((brackets, items)) = array(text, path)? else { return Ok(text.to_string()) };
    let mut out = text.to_string();
    // Back to front, so the ranges of the elements still to visit stay valid.
    for r in items.iter().rev() {
        let v: Value = serde_json::from_str(&to_strict_json(&text[r.clone()])).map_err(|e| e.to_string())?;
        if drop(&v) {
            out = cut(&out, brackets.start, r.start, r.end);
        }
    }
    Ok(out)
}

/// `text` without the member or element at `start..end` of the object or array opening at
/// `open`, and one adjacent comma, so the rest stays valid JSON. One alone on its line goes
/// with the line, which gives back the bytes from before it was added.
fn cut(text: &str, open: usize, start: usize, end: usize) -> String {
    let b = text.as_bytes();
    let mut after = end;
    skip(b, &mut after);
    let comma = (b.get(after) == Some(&b',')).then_some(after);
    let member_end = comma.map_or(end, |c| c + 1);
    let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
    let alone_before = b[line_start..start].iter().all(|c| matches!(c, b' ' | b'\t'));
    let mut tail = member_end;
    while matches!(b.get(tail), Some(b' ' | b'\t')) {
        tail += 1;
    }
    let line_end = match (b.get(tail), b.get(tail + 1)) {
        (Some(b'\n'), _) => Some(tail + 1),
        (Some(b'\r'), Some(b'\n')) => Some(tail + 2),
        _ => None,
    };
    // The last member leaves a comma behind on the one before it: that comma goes too.
    let prev = if comma.is_none() {
        last_significant(&b[open + 1..start])
            .filter(|&(_, c)| c == b',')
            .map(|(i, _)| open + 1 + i)
    } else {
        None
    };
    let mut out = String::with_capacity(text.len());
    match (line_end, alone_before) {
        (Some(line_end), true) => {
            match prev {
                Some(p) => {
                    out.push_str(&text[..p]);
                    out.push_str(&text[p + 1..line_start]);
                }
                None => out.push_str(&text[..line_start]),
            }
            out.push_str(&text[line_end..]);
        }
        _ => {
            // Inline: also take the whitespace after a comma, never a comment (it is the next member's).
            let mut to = member_end;
            while comma.is_some() && b.get(to).is_some_and(u8::is_ascii_whitespace) {
                to += 1;
            }
            out.push_str(&text[..prev.unwrap_or(start)]);
            out.push_str(&text[to..]);
        }
    }
    out
}

/// Drop comments and trailing commas: every edit's result must parse as strict JSON.
pub fn to_strict_json(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                let _ = string_value(text, &mut i);
                out.push_str(&text[start..i]);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b',' => {
                let mut j = i + 1;
                skip(b, &mut j);
                if matches!(b.get(j), Some(b'}') | Some(b']')) {
                    // trailing comma: drop it
                } else {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                // Whole characters: a byte-wise copy turns a BOM into `ï»¿`, which no parser takes.
                let ch = text[i..].chars().next().unwrap_or('\0');
                if ch != '\u{feff}' {
                    out.push(ch);
                }
                i += ch.len_utf8();
            }
        }
    }
    out
}

struct Member {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

fn first_value(text: &str) -> Result<usize, String> {
    // PowerShell 5.1 `Set-Content -Encoding UTF8` writes a BOM; it stays in the file, the edits go after it.
    let mut i = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    skip(text.as_bytes(), &mut i);
    if i < text.len() {
        Ok(i)
    } else {
        Err("empty document".into())
    }
}

/// Every member of the object starting at `start`, plus the offset of its `}`.
fn members(text: &str, start: usize) -> Result<(Vec<Member>, usize), String> {
    let b = text.as_bytes();
    let mut i = start;
    if b.get(i) != Some(&b'{') {
        return Err("not an object".into());
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        skip(b, &mut i);
        match b.get(i) {
            Some(b'}') => return Ok((out, i)),
            Some(b'"') => {}
            Some(_) => return Err("expected a quoted key".into()),
            None => return Err("unexpected end of input".into()),
        }
        let key_start = i;
        let key = string_value(text, &mut i)?;
        skip(b, &mut i);
        if b.get(i) != Some(&b':') {
            return Err("expected ':' after a key".into());
        }
        i += 1;
        skip(b, &mut i);
        let value_start = i;
        value(b, &mut i)?;
        out.push(Member {
            key,
            key_start,
            value_start,
            value_end: i,
        });
        skip(b, &mut i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => return Ok((out, i)),
            Some(_) => return Err("expected ',' or '}'".into()),
            None => return Err("unexpected end of input".into()),
        }
    }
}

/// The object that must hold `path`, which is the parent of the member to insert.
fn parent_object(text: &str, path: &[&str]) -> Result<usize, String> {
    let mut start = first_value(text)?;
    for key in &path[..path.len() - 1] {
        let (list, _) = members(text, start)?;
        match list.iter().find(|m| m.key == *key) {
            Some(m) if text.as_bytes().get(m.value_start) == Some(&b'{') => start = m.value_start,
            _ => return Err(format!("{key}: no such object")),
        }
    }
    Ok(start)
}

fn insert_member(text: &str, path: &[&str], value: &Value) -> Result<String, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    // Create the missing parents first, so a one-line `{}` becomes a nested object.
    if path.len() > 1 {
        let parent = &path[..path.len() - 1];
        if member(text, parent)?.is_none() {
            let with_parent = insert_member(text, parent, &Value::Object(serde_json::Map::new()))?;
            return insert_into_object(&with_parent, path, value);
        }
    }
    insert_into_object(text, path, value)
}

fn insert_into_object(text: &str, path: &[&str], value: &Value) -> Result<String, String> {
    let parent = parent_object(text, path)?;
    let b = text.as_bytes();
    let (list, close) = members(text, parent)?;
    let nl = eol(text);
    let close_indent = line_indent(text, close);
    let child_indent = list
        .iter()
        .map(|m| line_indent(text, m.key_start))
        .find(|indent| indent.len() > close_indent.len())
        .unwrap_or_else(|| format!("{close_indent}  "));
    let key = serde_json::to_string(path[path.len() - 1]).map_err(|e| e.to_string())?;
    let member_text = format!("{key}: {}", reindent(value, &child_indent, nl));
    let Some(last) = list.last() else {
        // Only whitespace or comments inside: the member gets its own line and `}` the next.
        let mut at = close;
        while at > parent + 1 && matches!(b[at - 1], b' ' | b'\t') {
            at -= 1;
        }
        let lead = if b[at - 1] == b'\n' { "" } else { nl };
        return Ok(format!(
            "{}{lead}{child_indent}{member_text}{nl}{close_indent}{}",
            &text[..at],
            &text[close..]
        ));
    };
    // The comma after the last value may sit behind a comment, so look past comments for it.
    let mut j = last.value_end;
    skip(b, &mut j);
    let comma = b.get(j) == Some(&b',');
    // The new member goes after the line the last one ends on: a comment on that line stays with it.
    let mut anchor = if comma { j + 1 } else { last.value_end };
    let mut k = anchor;
    loop {
        while matches!(b.get(k), Some(b' ' | b'\t')) {
            k += 1;
        }
        if b[k..].starts_with(b"//") {
            while k < b.len() && !matches!(b[k], b'\n' | b'\r') {
                k += 1;
            }
            anchor = k;
        } else if b[k..].starts_with(b"/*") {
            if let Some(e) = text[k..].find("*/").filter(|&e| !text[k..k + e].contains('\n')) {
                k += e + 2;
                anchor = k;
                continue;
            }
        }
        break;
    }
    let mut out = String::with_capacity(text.len() + member_text.len() + 8);
    if comma {
        out.push_str(&text[..anchor]);
        out.push_str(&format!("{nl}{child_indent}{member_text},"));
    } else {
        out.push_str(&text[..last.value_end]);
        out.push(',');
        out.push_str(&text[last.value_end..anchor]);
        out.push_str(&format!("{nl}{child_indent}{member_text}"));
    }
    out.push_str(&text[anchor..]);
    Ok(out)
}

/// The leading whitespace of the line `pos` sits on.
fn line_indent(text: &str, pos: usize) -> String {
    let bytes = text.as_bytes();
    let mut start = pos.min(bytes.len());
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    text[start..pos.min(bytes.len())]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// `\r\n` when the document uses it, else `\n`: new lines match the ones around them.
fn eol(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// `{}` (after any BOM and comments) when the text holds no value at all, else `None`.
fn empty_baseline(text: &str) -> Option<String> {
    if first_value(text).is_ok() {
        return None;
    }
    let (bom, rest) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    if rest.trim().is_empty() {
        return Some(format!("{bom}{{}}{}", if rest.is_empty() { "" } else { eol(text) }));
    }
    let sep = if rest.ends_with('\n') { "" } else { eol(text) };
    Some(format!("{bom}{rest}{sep}{{}}"))
}

/// `serde_json`'s pretty output, re-indented to sit at `indent`.
fn reindent(value: &Value, indent: &str, nl: &str) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    let mut out = String::with_capacity(pretty.len() + 8 * indent.len());
    for (i, line) in pretty.lines().enumerate() {
        if i == 0 {
            out.push_str(line);
            continue;
        }
        out.push_str(nl);
        out.push_str(indent);
        let lead = line.len() - line.trim_start_matches(' ').len();
        for _ in 0..lead / 2 {
            out.push_str("  ");
        }
        out.push_str(&line[lead..]);
    }
    out
}

fn skip(b: &[u8], i: &mut usize) {
    loop {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
        if b.get(*i) == Some(&b'/') {
            match b.get(*i + 1) {
                Some(b'/') => {
                    while *i < b.len() && b[*i] != b'\n' {
                        *i += 1;
                    }
                    continue;
                }
                Some(b'*') => {
                    *i += 2;
                    while *i + 1 < b.len() && !(b[*i] == b'*' && b[*i + 1] == b'/') {
                        *i += 1;
                    }
                    *i = (*i + 2).min(b.len());
                    continue;
                }
                _ => {}
            }
        }
        return;
    }
}

/// The last byte outside strings, whitespace and comments, and where it is.
fn last_significant(bytes: &[u8]) -> Option<(usize, u8)> {
    let mut i = 0;
    let mut last = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let mut j = i;
                if string_bytes(bytes, &mut j).is_err() {
                    return last;
                }
                last = Some((i, b'"'));
                i = j;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            c if c.is_ascii_whitespace() => i += 1,
            c => {
                last = Some((i, c));
                i += 1;
            }
        }
    }
    last
}

fn string_value(text: &str, i: &mut usize) -> Result<String, String> {
    let b = text.as_bytes();
    if b.get(*i) != Some(&b'"') {
        return Err("expected a string".into());
    }
    *i += 1;
    let mut out = String::new();
    while let Some(&c) = b.get(*i) {
        match c {
            b'"' => {
                *i += 1;
                return Ok(out);
            }
            b'\\' => {
                let escaped = *b.get(*i + 1).ok_or("unterminated escape")?;
                out.push(match escaped {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    other => other as char,
                });
                *i += 2;
            }
            other => {
                out.push(other as char);
                *i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

fn value(b: &[u8], i: &mut usize) -> Result<(), String> {
    skip(b, i);
    match b.get(*i) {
        Some(b'"') => {
            string_bytes(b, i)?;
            Ok(())
        }
        Some(b'{') => {
            *i += 1;
            loop {
                skip(b, i);
                match b.get(*i) {
                    Some(b'}') => {
                        *i += 1;
                        return Ok(());
                    }
                    Some(b'"') => {
                        let mut j = *i;
                        string_bytes(b, &mut j)?;
                        *i = j;
                        skip(b, i);
                        if b.get(*i) != Some(&b':') {
                            return Err("expected ':'".into());
                        }
                        *i += 1;
                        value(b, i)?;
                        skip(b, i);
                        match b.get(*i) {
                            Some(b',') => *i += 1,
                            Some(b'}') => {
                                *i += 1;
                                return Ok(());
                            }
                            _ => return Err("expected ',' or '}'".into()),
                        }
                    }
                    _ => return Err("expected a key".into()),
                }
            }
        }
        Some(b'[') => {
            *i += 1;
            loop {
                skip(b, i);
                match b.get(*i) {
                    Some(b']') => {
                        *i += 1;
                        return Ok(());
                    }
                    Some(_) => {
                        value(b, i)?;
                        skip(b, i);
                        match b.get(*i) {
                            Some(b',') => *i += 1,
                            Some(b']') => {
                                *i += 1;
                                return Ok(());
                            }
                            _ => return Err("expected ',' or ']'".into()),
                        }
                    }
                    None => return Err("unexpected end of input".into()),
                }
            }
        }
        Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') => {
            while let Some(c) = b.get(*i) {
                if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                    *i += 1;
                } else {
                    break;
                }
            }
            Ok(())
        }
        Some(_) => Err("unexpected character".into()),
        None => Err("unexpected end of input".into()),
    }
}

fn string_bytes(b: &[u8], i: &mut usize) -> Result<(), String> {
    if b.get(*i) != Some(&b'"') {
        return Err("expected a string".into());
    }
    *i += 1;
    while let Some(&c) = b.get(*i) {
        match c {
            b'"' => {
                *i += 1;
                return Ok(());
            }
            b'\\' => *i += 2,
            _ => *i += 1,
        }
    }
    Err("unterminated string".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_member_preserves_unrelated_bytes() {
        let src = "{\n  // keep me\n  \"a\": 1, /* and me */\n  \"mcp\": {\"servers\": {\"other\": {\"x\": 1},}},\n}\n";
        let out = set_member(src, &["mcp", "servers", "qx"], &json!({"type":"local"})).unwrap();
        assert!(out.starts_with("{\n  // keep me\n  \"a\": 1, /* and me */\n"));
        assert!(out.contains("\"other\": {\"x\": 1}"));
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["mcp"]["servers"]["qx"]["type"], "local");
    }

    #[test]
    fn set_member_creates_parents_and_replaces_in_place() {
        let out = set_member("{}", &["mcpServers", "qx"], &json!({"command":"x"})).unwrap();
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["mcpServers"]["qx"]["command"], "x");
        // root object, the created parent, and the value's own object
        assert_eq!(out.matches('{').count(), 3);

        let src = "{\n  \"a\": 1,\n  \"b\": {\n    \"c\": 2\n  }\n}\n";
        let out = set_member(src, &["b", "c"], &json!(3)).unwrap();
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["b"]["c"], 3);
        // Everything outside the replaced value is untouched.
        let before = &src[..src.find("\"c\": 2").unwrap()];
        assert!(out.starts_with(before.trim_end_matches(' ')));
        assert!(out.ends_with("\n}\n"));
    }

    #[test]
    fn malformed_text_is_an_error() {
        assert!(set_member("{\"a\": ", &["b"], &json!(1)).is_err());
        assert!(member("{\"a\" 1}", &["a"]).is_err());
    }

    #[test]
    fn member_finds_nested_values_and_misses_cleanly() {
        let src = r#"{"a":{"b":[1,2,{"c":true}]},"d":"x"}"#;
        let r = member(src, &["a", "b"]).unwrap().unwrap();
        assert_eq!(&src[r], "[1,2,{\"c\":true}]");
        let r = member(src, &["a", "b", "0"]).unwrap();
        assert!(r.is_none(), "an array index is not a member");
        assert!(member(src, &["nope"]).unwrap().is_none());
        assert!(member(src, &["d", "e"]).unwrap().is_none());
    }

    #[test]
    fn remove_member_keeps_the_rest_byte_identical() {
        let src = "{\n  \"a\": 1,\n  \"qx\": {\"x\": 1},\n  \"b\": 2\n}\n";
        let out = remove_member(src, &["qx"]).unwrap();
        assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": 2\n}\n");
        let src = "{\n  \"a\": 1,\n  \"qx\": {\"x\": 1}\n}\n";
        let out = remove_member(src, &["qx"]).unwrap();
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["a"], 1);
        assert!(v.get("qx").is_none());
        // Removing something absent changes nothing.
        assert_eq!(remove_member(src, &["zz"]).unwrap(), src);
    }

    #[test]
    fn remove_member_with_an_absent_parent_is_a_no_op() {
        let src = "{\"a\":1}";
        assert_eq!(remove_member(src, &["mcpServers", "qx"]).unwrap(), src);
        assert_eq!(remove_member("{}", &["mcp", "servers", "qx"]).unwrap(), "{}");
    }

    #[test]
    fn remove_member_keeps_comments_after_the_comma() {
        let src = "{\n  \"qx\": {\"x\":1},\n  // keep: important\n  \"b\": 2\n}";
        assert_eq!(remove_member(src, &["qx"]).unwrap(), "{\n  // keep: important\n  \"b\": 2\n}");
        let src = "{\n  \"qx\": 1,\n  /* keep */\n  \"b\": 2\n}";
        assert_eq!(remove_member(src, &["qx"]).unwrap(), "{\n  /* keep */\n  \"b\": 2\n}");
    }

    #[test]
    fn strict_json_drops_comments_and_trailing_commas() {
        let src = "{\n// c\n\"a\": 1, /* x */\n\"b\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&to_strict_json(src)).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"][1], 2);
        // A //-comment inside a string is not a comment.
        let v: Value = serde_json::from_str(&to_strict_json(r#"{"u":"http://x"}"#)).unwrap();
        assert_eq!(v["u"], "http://x");
        // A BOM (Notepad writes one) and non-ASCII outside strings survive as characters.
        let v: Value = serde_json::from_str(&to_strict_json("\u{feff}{\"a\": \"caf\u{e9}\" /* \u{4e2d} */}")).unwrap();
        assert_eq!(v["a"], "caf\u{e9}");
    }

    #[test]
    fn a_leading_bom_is_kept_and_edited_around() {
        let src = "\u{feff}{\n  \"a\": 1\n}\n";
        assert!(member(src, &["a"]).unwrap().is_some());
        let out = set_member(src, &["servers", "qx"], &json!({"type":"local"})).unwrap();
        assert!(out.starts_with('\u{feff}'), "{out:?}");
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["servers"]["qx"]["type"], "local");
        assert_eq!(v["a"], 1);
        let out = remove_member(&out, &["servers"]).unwrap();
        assert!(out.starts_with('\u{feff}') && !out.contains("servers"), "{out:?}");
    }

    #[test]
    fn a_comment_after_a_trailing_comma_is_not_a_second_comma() {
        let src = "{\n  \"servers\": {\n    \"other\": {\"x\": 1}, // keep me\n  },\n}\n";
        let out = set_member(src, &["servers", "qx"], &json!({"type":"local"})).unwrap();
        assert!(out.contains("// keep me"), "{out}");
        let strict = to_strict_json(&out);
        assert!(!strict.contains(",,"), "{strict}");
        let v: Value = serde_json::from_str(&strict).unwrap();
        assert_eq!(v["servers"]["qx"]["type"], "local");
        assert_eq!(v["servers"]["other"]["x"], 1);
    }

    #[test]
    fn arrays_grow_and_shrink_in_place() {
        let src = "{\n  \"h\": {\n    \"A\": [\n      {\"m\": \"x\"}\n    ]\n  }\n}\n";
        let out = push_element(src, &["h", "A"], &json!(2)).unwrap();
        assert_eq!(out, "{\n  \"h\": {\n    \"A\": [\n      {\"m\": \"x\"},\n      2\n    ]\n  }\n}\n");
        assert_eq!(elements(&out, &["h", "A"]).unwrap(), vec![json!({"m": "x"}), json!(2)]);
        assert_eq!(remove_elements(&out, &["h", "A"], |v| v == &json!(2)).unwrap(), src);
        let empty = remove_elements(&out, &["h", "A"], |_| true).unwrap();
        assert_eq!(elements(&empty, &["h", "A"]).unwrap(), Vec::<Value>::new());
        let refilled = push_element(&empty, &["h", "A"], &json!(1)).unwrap();
        assert_eq!(elements(&refilled, &["h", "A"]).unwrap(), vec![json!(1)]);

        // An absent array is created, next to the members already there.
        let out = push_element(src, &["h", "B"], &json!(1)).unwrap();
        let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
        assert_eq!(v["h"]["B"], json!([1]));
        assert_eq!(v["h"]["A"][0]["m"], "x");
        assert!(out.contains("    ]"), "{out}");
        assert!(push_element("{\"a\": 1}", &["a"], &json!(1)).is_err());
    }

    #[test]
    fn blank_or_bom_only_text_is_an_empty_document() {
        for blank in ["", "  \r\n", "\u{feff}", "\u{feff}\n"] {
            assert_eq!(member(blank, &["a"]).unwrap(), None, "{blank:?}");
            assert_eq!(remove_member(blank, &["a"]).unwrap(), blank, "{blank:?}");
            assert!(elements(blank, &["h", "A"]).unwrap().is_empty(), "{blank:?}");
            let out = set_member(blank, &["mcp", "qx"], &json!({"x": 1})).unwrap();
            assert_eq!(out.starts_with('\u{feff}'), blank.starts_with('\u{feff}'), "{out:?}");
            let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
            assert_eq!(v["mcp"]["qx"]["x"], 1, "{out:?}");
            let out = push_element(blank, &["h", "A"], &json!(1)).unwrap();
            assert_eq!(out.starts_with('\u{feff}'), blank.starts_with('\u{feff}'), "{out:?}");
            let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
            assert_eq!(v["h"]["A"], json!([1]), "{out:?}");
        }
    }

    #[test]
    fn a_new_member_goes_on_its_own_line_after_the_last_comma() {
        let add = |src: &str| set_member(src, &["b"], &json!(2)).unwrap();
        assert_eq!(add("{\n  \"a\": 1\n}\n"), "{\n  \"a\": 1,\n  \"b\": 2\n}\n");
        assert_eq!(add("{\n  \"a\": 1,\n}\n"), "{\n  \"a\": 1,\n  \"b\": 2,\n}\n");
        assert_eq!(add("{\n  \"a\": 1 // c\n}\n"), "{\n  \"a\": 1, // c\n  \"b\": 2\n}\n");
        assert_eq!(add("{\n  \"a\": 1, // c\n}\n"), "{\n  \"a\": 1, // c\n  \"b\": 2,\n}\n");
        let out = add("{\n}\n");
        assert_eq!(out, "{\n  \"b\": 2\n}\n");
        let v: Value = serde_json::from_str(&to_strict_json(&add("{}"))).unwrap();
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn crlf_text_gets_crlf_inserts() {
        let src = "{\r\n  \"a\": 1\r\n}\r\n";
        let out = set_member(src, &["b"], &json!(2)).unwrap();
        assert_eq!(out, "{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}\r\n");
        let out = set_member(src, &["m", "n"], &json!({"x": 1})).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'), "a bare LF in {out:?}");
        let out = push_element("{\r\n  \"h\": [\r\n    1\r\n  ]\r\n}\r\n", &["h"], &json!(2)).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'), "a bare LF in {out:?}");
        assert_eq!(elements(&out, &["h"]).unwrap(), vec![json!(1), json!(2)]);
    }

    #[test]
    fn removing_the_last_member_gives_back_the_bytes_before_it_was_added() {
        for src in [
            "{\n  \"a\": 1 // c\n}\n",
            "{\n  \"a\": 1\n}\n",
            "{\n  \"a\": 1, // c\n}\n",
            "{\r\n  \"a\": 1 /* c */\r\n}\r\n",
        ] {
            let added = set_member(src, &["qx"], &json!(2)).unwrap();
            assert_ne!(added, src);
            assert_eq!(remove_member(&added, &["qx"]).unwrap(), src, "{added:?}");
        }
    }

    #[test]
    fn prune_empty_removes_only_containers_with_nothing_in_them() {
        let src = "{\n  \"a\": 1,\n  \"m\": {\n    \"s\": {}\n  }\n}\n";
        assert_eq!(prune_empty(src, &["m", "s"]).unwrap(), "{\n  \"a\": 1\n}\n");
        // The root collapses to `{}`, keeping what surrounds it.
        assert_eq!(prune_empty("{\n  \"m\": {}\n}\n", &["m"]).unwrap(), "{}\n");
        assert_eq!(prune_empty("\u{feff}{\n  \"m\": []\n}\n", &["m"]).unwrap(), "\u{feff}{}\n");
        // Anything in there, a member or a comment, stops the pruning.
        for keep in [
            "{\"m\": {\"s\": 1}}",
            "{\"m\": {/* keep */}}",
            "{\"m\": [0]}",
            "{\"m\": 5}",
            "{\"a\": 1}",
        ] {
            assert_eq!(prune_empty(keep, &["m"]).unwrap(), keep);
        }
        assert_eq!(prune_empty("{\"m\": {\"s\": {}, \"t\": 1}}", &["m", "s"]).unwrap(), "{\"m\": {\"t\": 1}}");
        assert!(prune_empty("{\"a\" 1}", &["a"]).is_err());
    }

    #[test]
    fn replace_element_rewrites_one_array_item_in_place() {
        let src = "{\n  \"h\": {\n    \"A\": [\n      {\"m\": \"x\"},\n      {\"k\": 1}\n    ]\n  }\n}\n";
        let out = replace_element(src, &["h", "A"], 1, &json!({"k": 2})).unwrap();
        assert_eq!(elements(&out, &["h", "A"]).unwrap(), vec![json!({"m": "x"}), json!({"k": 2})]);
        let head = &src[..src.find("{\"k\": 1}").unwrap()];
        assert!(out.starts_with(head), "{out}");
        assert!(out.ends_with("\n    ]\n  }\n}\n"), "{out}");
        assert!(replace_element(src, &["h", "A"], 2, &json!(1)).is_err());
        assert!(replace_element(src, &["h", "B"], 0, &json!(1)).is_err());
    }
}
