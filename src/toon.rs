//! A small reader for the subset of TOON that `no-mistakes axi` prints.
//!
//! Supported shapes, each nested by two-space indentation:
//!
//! ```text
//! key: scalar            a scalar, optionally "double quoted"
//! key:                   an object whose fields follow, indented
//! key[N]{a,b,c}:         a table whose N comma-separated rows follow, indented
//! key[N]:                a list whose N values follow, one per line, indented
//! ```
//!
//! Anything else is skipped rather than rejected, so a newer `no-mistakes`
//! that adds fields keeps working.

#[derive(Debug, Clone, PartialEq)]
pub enum Toon {
    Scalar(String),
    Object(Vec<(String, Toon)>),
    Table {
        fields: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    List(Vec<String>),
}

impl Toon {
    /// Field of an object by key.
    pub fn get(&self, key: &str) -> Option<&Toon> {
        match self {
            Toon::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Scalar value of an object field.
    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Toon::Scalar(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Rows of a table field as maps from column name to value.
    pub fn table(&self, key: &str) -> Vec<Vec<(String, String)>> {
        match self.get(key) {
            Some(Toon::Table { fields, rows }) => rows
                .iter()
                .map(|row| {
                    fields
                        .iter()
                        .cloned()
                        .zip(row.iter().cloned().chain(std::iter::repeat(String::new())))
                        .collect()
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Look up a column in a table row produced by [`Toon::table`].
pub fn col<'a>(row: &'a [(String, String)], name: &str) -> &'a str {
    row.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

struct Line<'a> {
    indent: usize,
    text: &'a str,
}

/// Parse a TOON document into its top-level object.
pub fn parse(input: &str) -> Toon {
    let lines: Vec<Line> = input
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let trimmed = l.trim_start_matches(' ');
            Line {
                indent: l.len() - trimmed.len(),
                text: trimmed.trim_end(),
            }
        })
        .collect();
    let mut pos = 0;
    Toon::Object(parse_object(&lines, &mut pos, 0))
}

fn parse_object(lines: &[Line], pos: &mut usize, indent: usize) -> Vec<(String, Toon)> {
    let mut out = Vec::new();
    while *pos < lines.len() {
        let line = &lines[*pos];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            // Unexpected deeper line: skip it.
            *pos += 1;
            continue;
        }
        *pos += 1;
        let Some((head, rest)) = split_key(line.text) else {
            continue;
        };
        let rest = rest.trim();
        if let Some((name, count, fields)) = parse_header(head) {
            let child_indent = next_indent(lines, *pos, indent);
            let mut rows = Vec::new();
            while *pos < lines.len() && lines[*pos].indent >= child_indent && child_indent > indent
            {
                if lines[*pos].indent == child_indent {
                    rows.push(lines[*pos].text);
                }
                *pos += 1;
            }
            let _ = count;
            let value = match fields {
                Some(fields) => Toon::Table {
                    fields,
                    rows: rows.iter().map(|r| split_row(r)).collect(),
                },
                None => Toon::List(
                    rows.iter()
                        .map(|r| unquote(r.strip_prefix("- ").unwrap_or(r)))
                        .collect(),
                ),
            };
            out.push((name, value));
        } else if rest.is_empty() {
            let child_indent = next_indent(lines, *pos, indent);
            if child_indent > indent {
                out.push((
                    head.to_string(),
                    Toon::Object(parse_object(lines, pos, child_indent)),
                ));
            } else {
                out.push((head.to_string(), Toon::Scalar(String::new())));
            }
        } else {
            out.push((head.to_string(), Toon::Scalar(unquote(rest))));
        }
    }
    out
}

fn next_indent(lines: &[Line], pos: usize, indent: usize) -> usize {
    lines.get(pos).map(|l| l.indent).unwrap_or(indent)
}

/// Split `key: value` at the first colon that is outside quotes and brackets.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut in_quote = false;
    for (i, ch) in text.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            '[' | '{' if !in_quote => depth += 1,
            ']' | '}' if !in_quote => depth -= 1,
            ':' if !in_quote && depth == 0 => return Some((&text[..i], &text[i + 1..])),
            _ => {}
        }
    }
    None
}

/// `name[N]{a,b}` or `name[N]`.
fn parse_header(head: &str) -> Option<(String, usize, Option<Vec<String>>)> {
    let open = head.find('[')?;
    let close = head[open..].find(']')? + open;
    let name = head[..open].to_string();
    let count = head[open + 1..close].parse().ok()?;
    let after = &head[close + 1..];
    let fields = if after.starts_with('{') && after.ends_with('}') {
        Some(
            after[1..after.len() - 1]
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
        )
    } else if after.is_empty() {
        None
    } else {
        return None;
    };
    Some((name, count, fields))
}

/// Split one table row on commas outside double quotes.
pub fn split_row(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = row.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if in_quote => {
                if let Some(next) = chars.next() {
                    cur.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            '"' => in_quote = !in_quote,
            ',' if !in_quote => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        split_row(s).concat()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_objects_tables_and_lists() {
        let doc = parse(
            "run:\n  id: \"01ABC\"\n  status: running\n  steps[2]{step,status,findings,duration_ms}:\n    intent,completed,0,25\n    ci,running,0,0\n  active_steps[1]{step,last_activity}:\n    ci,\"quiet 3m ago: log: a, b\"\nhelp[2]:\n  Run `x`\n  \"Run y\"\n",
        );
        let run = doc.get("run").unwrap();
        assert_eq!(run.str("id"), Some("01ABC"));
        assert_eq!(run.str("status"), Some("running"));
        let steps = run.table("steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(col(&steps[1], "step"), "ci");
        assert_eq!(col(&steps[0], "duration_ms"), "25");
        let active = run.table("active_steps");
        assert_eq!(col(&active[0], "last_activity"), "quiet 3m ago: log: a, b");
        assert_eq!(
            doc.get("help"),
            Some(&Toon::List(vec!["Run `x`".into(), "Run y".into()]))
        );
    }

    #[test]
    fn tolerates_unknown_shapes() {
        let doc = parse("weird line without colon\nkey: value\n   odd: indent\n");
        assert_eq!(doc.str("key"), Some("value"));
    }
}
