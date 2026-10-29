//! Codex block patches and conservative three-level text matching.
#[derive(Debug)]
pub(crate) enum Operation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        moved: Option<String>,
        hunks: Vec<(String, String, bool)>,
    },
}
pub(crate) fn parse(input: &str) -> Result<Vec<Operation>, String> {
    let lines = input.lines().collect::<Vec<_>>();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err("patch requires *** Begin Patch and *** End Patch".into());
    }
    let mut i = 1;
    let mut operations = Vec::new();
    while i + 1 < lines.len() {
        if let Some(path) = lines[i].strip_prefix("*** Add File: ") {
            let path = path.to_owned();
            i += 1;
            let mut content = String::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let line = lines[i]
                    .strip_prefix('+')
                    .ok_or("Add File lines require +")?;
                content.push_str(line);
                content.push('\n');
                i += 1;
            }
            operations.push(Operation::Add { path, content });
        } else if let Some(path) = lines[i].strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete { path: path.into() });
            i += 1;
        } else if let Some(path) = lines[i].strip_prefix("*** Update File: ") {
            let path = path.to_owned();
            i += 1;
            let mut moved = None;
            if let Some(target) = lines.get(i).and_then(|s| s.strip_prefix("*** Move to: ")) {
                moved = Some(target.into());
                i += 1;
            }
            let mut hunks = Vec::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                if !lines[i].starts_with("@@") {
                    return Err("Update File requires @@ hunk headers".into());
                }
                i += 1;
                let (mut old, mut new) = (String::new(), String::new());
                let mut eof = false;
                while i + 1 < lines.len()
                    && !lines[i].starts_with("@@")
                    && (!lines[i].starts_with("*** ") || lines[i] == "*** End of File")
                {
                    let line = lines[i];
                    i += 1;
                    if line == "*** End of File" {
                        eof = true;
                        break;
                    }
                    let (prefix, text) = line
                        .split_at_checked(1)
                        .ok_or("patch context must start with space, + or -")?;
                    match prefix {
                        " " => {
                            old.push_str(text);
                            old.push('\n');
                            new.push_str(text);
                            new.push('\n');
                        }
                        "-" => {
                            old.push_str(text);
                            old.push('\n');
                        }
                        "+" => {
                            new.push_str(text);
                            new.push('\n');
                        }
                        _ => return Err("invalid patch hunk line".into()),
                    }
                }
                hunks.push((old, new, eof));
            }
            if hunks.is_empty() {
                return Err("Update File requires at least one hunk".into());
            }
            operations.push(Operation::Update { path, moved, hunks });
        } else {
            return Err(format!("unknown patch directive: {}", lines[i]));
        }
    }
    if operations.is_empty() {
        return Err("patch has no operations".into());
    }
    Ok(operations)
}
fn normalize(text: &str, level: u8) -> (String, Vec<(usize, usize)>) {
    let mut result = String::new();
    let mut positions = Vec::new();
    for (index, c) in text.char_indices() {
        if level == 2
            && matches!(c, ' ' | '\t' | '\r')
            && text[index..]
                .split('\n')
                .next()
                .unwrap_or("")
                .trim_end()
                .is_empty()
        {
            continue;
        }
        let c = if level >= 1 {
            match c {
                '‘' | '’' => '\'',
                '“' | '”' => '"',
                '–' | '—' | '−' => '-',
                '\u{a0}' => ' ',
                _ => c,
            }
        } else {
            c
        };
        result.push(c);
        positions.extend(std::iter::repeat_n(
            (
                index,
                index + text[index..].chars().next().unwrap().len_utf8(),
            ),
            c.len_utf8(),
        ));
    }
    (result, positions)
}
pub(crate) fn replace(content: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_text must not be empty".into());
    }
    for level in 0..3 {
        let (haystack, map) = normalize(content, level);
        let (needle, _) = normalize(old, level);
        if needle.is_empty() {
            return Err("old_text contains no matchable text".into());
        }
        let matches = haystack
            .match_indices(&needle)
            .map(|(i, _)| (map[i].0, map[i + needle.len() - 1].1))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            let lines = matches
                .iter()
                .map(|(i, _)| content[..*i].bytes().filter(|b| *b == b'\n').count() + 1)
                .collect::<Vec<_>>();
            return Err(format!("ambiguous match at lines {lines:?}"));
        }
        if let Some((start, end)) = matches.first() {
            let mut result = content.to_owned();
            result.replace_range(*start..*end, new);
            return Ok(result);
        }
    }
    Err("old_text was not found at any supported matching level".into())
}
pub(crate) fn apply_hunks(
    content: &str,
    hunks: &[(String, String, bool)],
) -> Result<String, String> {
    let mut result = content.to_owned();
    for (old, new, eof) in hunks {
        if old.is_empty() {
            result.push_str(new);
            continue;
        }
        let (old, new) = if !result.ends_with('\n') && old.ends_with('\n') {
            (old.trim_end_matches('\n'), new.trim_end_matches('\n'))
        } else {
            (old.as_str(), new.as_str())
        };
        if *eof && !result.ends_with(old) {
            return Err("End of File context does not match file end".into());
        }
        result = replace(&result, old, new)?;
    }
    Ok(result)
}
