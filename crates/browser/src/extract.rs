//! Readability-style HTML extraction for light reading (`web_read`).
//!
//! Pure string extraction without external HTML crates: title, headings,
//! paragraph fragments and Markdown output with byte offsets. Browser
//! fallback rendering is injected by the caller; this module never
//! performs network or Chrome I/O.

use serde::{Deserialize, Serialize};

/// One addressable fragment of extracted Markdown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragment {
    pub id: String,
    pub start: usize,
    pub end: usize,
    pub heading_path: Vec<String>,
    pub text: String,
}

/// Extracted document with Markdown, fragments and render-heuristic signals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub title: String,
    pub markdown: String,
    pub fragments: Vec<Fragment>,
    pub text_len: usize,
    pub markup_len: usize,
    pub script_count: usize,
    pub has_empty_mount: bool,
    pub asks_js: bool,
    pub has_embedded_data: bool,
}

/// Extract title, headings and body text from raw HTML.
pub fn extract_document(html: &str) -> Extracted {
    let markup_len = html.len();
    let title = extract_title(html);
    let script_count = count_tag(html, "script");
    let has_embedded_data = has_embedded_data(html);
    let asks_js = asks_javascript(html);
    let body = isolate_body(html);
    let cleaned = strip_ignorable(&body);
    let blocks = blocks_to_markdown(&cleaned);
    let markdown = normalize_markdown(&blocks.join("\n\n"));
    let fragments = fragmentize(&blocks);
    let text_len = markdown.len();
    let has_empty_mount = has_empty_mount(html, text_len);
    Extracted {
        title,
        markdown,
        fragments,
        text_len,
        markup_len,
        script_count,
        has_empty_mount,
        asks_js,
        has_embedded_data,
    }
}

/// Decide whether plain HTTP extraction is sufficient or a browser render is needed.
///
/// Mirrors architecture 14.2 step 4: small visible text with several scripts,
/// tiny text/markup ratio, an empty SPA mount with little text, or a
/// `<noscript>` asking for JavaScript escalates to the browser. Embedded
/// data (`__NEXT_DATA__`, `__NUXT__`, JSON-LD) avoids escalation.
pub fn needs_browser_render(doc: &Extracted) -> Option<String> {
    if doc.has_embedded_data {
        return None;
    }
    if doc.text_len < 500 && doc.script_count >= 3 {
        return Some("spa_shell".to_owned());
    }
    if doc.markup_len > 0 {
        let ratio = doc.text_len as f64 / doc.markup_len as f64;
        if ratio < 0.02 && doc.text_len < 2000 {
            return Some("low_text_ratio".to_owned());
        }
    }
    if doc.has_empty_mount {
        return Some("empty_mount".to_owned());
    }
    if doc.asks_js {
        return Some("requires_javascript".to_owned());
    }
    None
}

/// True when the page carries embedded JSON used by SPA frameworks.
pub fn has_embedded_data(html: &str) -> bool {
    html.contains("__NEXT_DATA__")
        || html.contains("__NUXT__")
        || html.contains("application/ld+json")
}

fn extract_title(html: &str) -> String {
    let lower = html.to_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let rest = &html[start..];
    let Some(gt) = rest.find('>') else {
        return String::new();
    };
    let after = &rest[gt + 1..];
    let lower_after = after.to_lowercase();
    let Some(end) = lower_after.find("</title>") else {
        return String::new();
    };
    collapse_ws(&decode_entities(&strip_tags(&after[..end])))
        .trim()
        .to_owned()
}

fn isolate_body(html: &str) -> String {
    let lower = html.to_lowercase();
    if let Some(start) = lower.find("<body")
        && let Some(gt) = html[start..].find('>')
    {
        let content = &html[start + gt + 1..];
        if let Some(end) = content.to_lowercase().find("</body>") {
            return content[..end].to_owned();
        }
        return content.to_owned();
    }
    html.to_owned()
}

fn strip_ignorable(html: &str) -> String {
    let mut out = html.to_owned();
    for tag in ["script", "style", "nav", "footer", "aside"] {
        out = remove_elements(&out, tag);
    }
    // Remove comments.
    while let Some(start) = out.find("<!--") {
        let Some(end) = out[start..].find("-->") else {
            break;
        };
        out.replace_range(start..start + end + 3, " ");
    }
    out
}

fn remove_elements(html: &str, tag: &str) -> String {
    let lower_tag_open = format!("<{tag}");
    let lower_tag_close = format!("</{tag}>");
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let lower = rest.to_lowercase();
        let Some(open) = lower.find(&lower_tag_open) else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..open]);
        let after_open = &rest[open..];
        let Some(close) = after_open.to_lowercase().find(&lower_tag_close) else {
            break;
        };
        rest = &after_open[close + lower_tag_close.len()..];
    }
    result
}

fn blocks_to_markdown(html: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut heading_path: Vec<String> = Vec::new();
    let mut pos = 0;
    let bytes = html.as_bytes();
    while pos < bytes.len() {
        if bytes[pos] == b'<' {
            if let Some(end) = html[pos..].find('>') {
                let tag_text = html[pos + 1..pos + end].to_lowercase();
                let tag_name: String = tag_text
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('/')
                    .to_owned();
                let is_heading =
                    matches!(tag_name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
                let is_block = is_heading
                    || matches!(
                        tag_name.as_str(),
                        "p" | "li" | "pre" | "blockquote" | "tr" | "br"
                    );
                if is_block {
                    let close = format!("</{tag_name}>");
                    let after = &html[pos + end + 1..];
                    let mut consumed = end + 1;
                    let text = if tag_name == "br" {
                        String::new()
                    } else if let Some(close_pos) = after.to_lowercase().find(&close) {
                        consumed += close_pos + close.len();
                        decode_entities(&strip_tags(&after[..close_pos]))
                    } else {
                        consumed = bytes.len() - pos;
                        decode_entities(&strip_tags(after))
                    };
                    let text = collapse_ws(&text);
                    let text = text.trim().to_owned();
                    if !text.is_empty() {
                        if is_heading {
                            let level: usize = tag_name[1..].parse().unwrap_or(2);
                            heading_path.truncate(level.saturating_sub(1));
                            if heading_path.len() >= level {
                                heading_path.truncate(level - 1);
                            }
                            heading_path.push(text.clone());
                            blocks.push(format!("{} {text}", "#".repeat(level.min(6))));
                        } else if tag_name == "li" {
                            blocks.push(format!("- {text}"));
                        } else if tag_name == "pre" {
                            blocks.push(format!("```\n{text}\n```"));
                        } else if tag_name == "blockquote" {
                            blocks.push(format!("> {text}"));
                        } else {
                            let with_links = absolutize_links(after, &text);
                            blocks.push(with_links);
                        }
                    }
                    // Keep heading path for subsequent paragraphs.
                    let _ = &heading_path;
                    pos += consumed;
                    continue;
                }
                pos += end + 1;
            } else {
                break;
            }
        } else {
            // Raw text between tags: keep only if substantial.
            let Some(next) = html[pos..].find('<') else {
                let tail = collapse_ws(&decode_entities(&html[pos..]));
                if tail.trim().len() >= 24 {
                    blocks.push(tail.trim().to_owned());
                }
                break;
            };
            let chunk = collapse_ws(&decode_entities(&html[pos..pos + next]));
            if chunk.trim().len() >= 24 {
                blocks.push(chunk.trim().to_owned());
            }
            pos += next;
        }
    }
    // Fallback: whole body as paragraphs when no blocks were found.
    if blocks.is_empty() {
        let text = collapse_ws(&decode_entities(&strip_tags(html)));
        let text = text.trim().to_owned();
        if !text.is_empty() {
            blocks.push(text);
        }
    }
    // Attach heading context implicitly via fragmentize; blocks already carry headings.
    blocks
}

fn absolutize_links(_html_after: &str, text: &str) -> String {
    text.to_owned()
}

fn fragmentize(blocks: &[String]) -> Vec<Fragment> {
    let mut fragments = Vec::new();
    let mut offset = 0;
    let mut heading_path: Vec<String> = Vec::new();
    let mut markdown = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            markdown.push_str("\n\n");
            offset += 2;
        }
        if let Some(heading) = block.strip_prefix("# ") {
            heading_path.clear();
            heading_path.push(heading.to_owned());
        } else if let Some(heading) = block.strip_prefix("## ") {
            heading_path.truncate(1.min(heading_path.len()));
            heading_path.push(heading.to_owned());
        } else if block.starts_with('#') {
            let text = block.trim_start_matches('#').trim().to_owned();
            heading_path.push(text);
        }
        let start = offset;
        markdown.push_str(block);
        offset += block.len();
        fragments.push(Fragment {
            id: format!("p{}", fragments.len() + 1),
            start,
            end: offset,
            heading_path: heading_path.clone(),
            text: block.clone(),
        });
    }
    fragments
}

fn normalize_markdown(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in input.split('\n') {
        let trimmed = collapse_ws(line);
        if trimmed.trim().is_empty() {
            if lines.last().is_some_and(|l: &String| l.is_empty()) {
                continue;
            }
            lines.push(String::new());
        } else {
            lines.push(trimmed.trim().to_owned());
        }
    }
    let joined = lines.join("\n");
    joined.trim().to_owned()
}

fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_ws(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

fn count_tag(html: &str, tag: &str) -> usize {
    html.to_lowercase().matches(&format!("<{tag}")).count()
}

fn asks_javascript(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("please enable javascript")
        || lower.contains("requires javascript")
        || lower.contains("enable javascript to")
}

fn has_empty_mount(html: &str, text_len: usize) -> bool {
    if text_len >= 500 {
        return false;
    }
    let lower = html.to_lowercase();
    for marker in ["id=\"root\"", "id=\"app\"", "id=\"__next\"", "ng-app"] {
        if lower.contains(marker) && text_len < 500 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_and_fragments() {
        let html = "<html><head><title>Example</title></head><body><h1>Hi</h1><p>First para.</p><p>Second para.</p></body></html>";
        let doc = extract_document(html);
        assert_eq!(doc.title, "Example");
        assert!(doc.markdown.contains("First para."));
        assert!(doc.fragments.len() >= 3);
        for f in &doc.fragments {
            assert_eq!(&doc.markdown[f.start..f.end], f.text);
        }
    }

    #[test]
    fn spa_shell_needs_browser() {
        let html = "<html><head></head><body><div id=\"root\"></div><script>1</script><script>2</script><script>3</script><p>hi</p></body></html>";
        let doc = extract_document(html);
        assert!(needs_browser_render(&doc).is_some());
    }

    #[test]
    fn embedded_data_avoids_browser() {
        let html = "<html><body><div id=\"root\">x</div><script id=\"__NEXT_DATA__\">{\"a\":1}</script><p>hello world content here, enough text to be useful for the reader path</p></body></html>";
        let doc = extract_document(html);
        assert!(doc.has_embedded_data);
        assert!(needs_browser_render(&doc).is_none());
    }
}
