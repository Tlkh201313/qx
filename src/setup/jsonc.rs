//! Byte-preserving JSONC editing for the CLI configs that allow comments.
//!
//! Rewriting a config with `serde_json` would reorder keys, drop comments and drop
//! trailing commas — and these files are hand-edited. Every function here edits the
//! text in place: bytes outside the member being changed are copied verbatim.

use serde_json::Value;
use std::ops::Range;

/// The byte range of the value at `path`, or `None` when it is absent.
pub fn member(text: &str, path: &[&str]) -> Result<Option<Range<usize>>, String> {
    let mut start = first_value(text)?;
    for (depth, key) in path.iter().enumerate() {
        let (list, close) = members(text, start)?;
        let Some(hit) = list.iter().find(|m| m.key == *key) else {
            return Ok(None);
        };
        if depth + 1 == path.len() {
            return Ok(Some(hit.value_start..hit.value_end));
        }
        let bytes = text.as_bytes();
        if bytes.get(hit.value_start) != Some(&b'{') {
            // A non-object on the way down means the member we want cannot exist.
            let _ = close;
            return Ok(None);
        }
        start = hit.value_start;
    }
    // An empty path means the whole document's value.
    let mut end = start;
    value(text.as_bytes(), &mut end, true)?;
    Ok(Some(start..end))
}

/// Replace the value at `path`, or insert the member (creating parent objects).
pub fn set_member(text: &str, path: &[&str], value: &Value) -> Result<String, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if let Some(range) = member(text, path)? {
        let current = text[range.clone()].trim();
        if current == serde_json::to_string(value).unwrap_or_default().as_str() {
            // Same bytes: nothing to do, so repeated runs stay Unchanged.
            return Ok(text.to_string());
        }
        let indent = indent_at(text, range.start);
        let mut out = String::with_capacity(text.len() + 64);
        out.push_str(&text[..range.start]);
        out.push_str(&reindent(value, &indent));
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
    let parent_start = parent_object(text, path)?;
    let (list, _close) = members(text, parent_start)?;
    let key = path[path.len() - 1];
    let Some(index) = list.iter().position(|m| m.key == key) else {
        return Ok(text.to_string());
    };
    let target = &list[index];
    // Drop one adjacent comma so the remaining list stays valid JSON.
    let mut start = target.key_start;
    let mut end = target.value_end;
    let bytes = text.as_bytes();
    let mut after = end;
    skip(bytes, &mut after);
    if bytes.get(after) == Some(&b',') {
        // Also take the whitespace that followed the comma, but leave the next member's
        // own indentation alone.
        let mut tail = after + 1;
        skip(bytes, &mut tail);
        end = tail;
    } else if index > 0 {
        // Last member: remove the comma that separated it from the previous one.
        let mut back = start;
        while back > parent_start {
            back -= 1;
            if bytes[back] == b',' {
                start = back;
                break;
            }
            if !bytes[back].is_ascii_whitespace() {
                break;
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    Ok(out)
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
            other => {
                out.push(other as char);
                i += 1;
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
    let mut i = 0;
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
        value(b, &mut i, true)?;
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
    let close_indent = line_indent(text, close);
    let child_indent = list
        .iter()
        .map(|m| line_indent(text, m.key_start))
        .find(|indent| indent.len() > close_indent.len())
        .unwrap_or_else(|| format!("{close_indent}  "));
    // What precedes the closing brace decides the separator: an empty object needs a
    // leading newline, a trailing comma needs none, anything else needs a comma.
    let mut last = close;
    while last > parent + 1 && b[last - 1].is_ascii_whitespace() {
        last -= 1;
    }
    let empty = list.is_empty();
    let trailing_comma = !empty && b.get(last.wrapping_sub(1)) == Some(&b',');
    let key = serde_json::to_string(path[path.len() - 1]).map_err(|e| e.to_string())?;
    let mut member_text = String::new();
    if !empty && !trailing_comma {
        member_text.push(',');
    }
    member_text.push('\n');
    member_text.push_str(&child_indent);
    member_text.push_str(&key);
    member_text.push_str(": ");
    member_text.push_str(&reindent(value, &child_indent));
    if empty {
        member_text.push('\n');
        member_text.push_str(&close_indent);
    }
    let mut out = String::with_capacity(text.len() + member_text.len());
    out.push_str(&text[..close]);
    out.push_str(&member_text);
    out.push_str(&text[close..]);
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

/// Indentation to give the first line of a value replaced in place.
fn indent_at(text: &str, value_start: usize) -> String {
    line_indent(text, value_start)
}

/// `serde_json`'s pretty output, re-indented to sit at `indent`.
fn reindent(value: &Value, indent: &str) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    let mut out = String::with_capacity(pretty.len() + 8 * indent.len());
    for (i, line) in pretty.lines().enumerate() {
        if i == 0 {
            out.push_str(line);
            continue;
        }
        out.push('\n');
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

fn value(b: &[u8], i: &mut usize, allow_comments: bool) -> Result<(), String> {
    if allow_comments {
        skip(b, i);
    }
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
                        value(b, i, allow_comments)?;
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
                        value(b, i, allow_comments)?;
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
        assert!(set_member("", &["a"], &json!(1)).is_err());
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
    fn strict_json_drops_comments_and_trailing_commas() {
        let src = "{\n// c\n\"a\": 1, /* x */\n\"b\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&to_strict_json(src)).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"][1], 2);
        // A //-comment inside a string is not a comment.
        let v: Value = serde_json::from_str(&to_strict_json(r#"{"u":"http://x"}"#)).unwrap();
        assert_eq!(v["u"], "http://x");
    }
}
