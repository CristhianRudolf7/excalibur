use super::*;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};
pub(super) struct CachedTree {
    language: Language,
    source: String,
    tree: Tree,
}
pub(super) struct Reference {
    pub id: String,
    pub source: String,
    pub kind: String,
    pub target: String,
    pub path: Option<String>,
    pub offset: usize,
}
pub(super) struct ParsedFile {
    pub mode: ParseMode,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub references: Vec<Reference>,
    pub issues: Vec<SyntaxIssue>,
    pub changed_ranges: Vec<Range>,
    pub redacted: usize,
}
struct Extract<'a> {
    path: &'a str,
    source: &'a str,
    language: Language,
    redactor: &'a Redactor,
    out: ParsedFile,
    file: String,
}
pub(super) fn parse(
    path: &str,
    language: Language,
    source: String,
    old: Option<CachedTree>,
    redactor: &Redactor,
    file: String,
) -> Result<(ParsedFile, CachedTree)> {
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|_| invalid("incompatible source grammar ABI"))?;
    let mut old = old.filter(|old| old.language == language);
    let mode = if old.is_some() {
        ParseMode::Incremental
    } else {
        ParseMode::Cold
    };
    if let Some(old) = &mut old {
        old.tree.edit(&edit(&old.source, &source));
    }
    let tree = parser
        .parse(&source, old.as_ref().map(|old| &old.tree))
        .ok_or_else(|| invalid("source parser did not return a tree"))?;
    let changed_ranges = old
        .as_ref()
        .map(|old| {
            old.tree
                .changed_ranges(&tree)
                .map(|r| Range {
                    start: position(&source, r.start_byte),
                    end: position(&source, r.end_byte),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut extract = Extract {
        path,
        source: &source,
        language,
        redactor,
        out: ParsedFile {
            mode,
            nodes: vec![],
            edges: vec![],
            references: vec![],
            issues: vec![],
            changed_ranges,
            redacted: 0,
        },
        file: file.clone(),
    };
    extract.add("file", path, 0, source.len(), "", json!({}));
    let stem = Path::new(path)
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or(path);
    let stem = if matches!(stem, "mod" | "__init__" | "index") {
        Path::new(path)
            .parent()
            .and_then(Path::file_name)
            .and_then(|v| v.to_str())
            .unwrap_or(stem)
    } else {
        stem
    };
    let kind = if language == Language::Markdown {
        "document"
    } else {
        "module"
    };
    let owner = extract
        .add(kind, stem, 0, source.len(), "", json!({}))
        .unwrap_or(file.clone());
    extract.edge(&file, &owner, "defines", "syntactic");
    extract.walk(tree.root_node(), &owner, stem, false, 0)?;
    if language == Language::Markdown {
        let mut inline = Parser::new();
        inline
            .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
            .map_err(|_| invalid("incompatible markdown inline grammar"))?;
        if let Some(tree) = inline.parse(&source, None) {
            extract.markdown_inline(tree.root_node(), &owner, 0)?;
        }
    }
    let out = extract.out;
    Ok((
        out,
        CachedTree {
            language,
            source,
            tree,
        },
    ))
}
fn edit(old: &str, new: &str) -> InputEdit {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = old.as_bytes()[start..]
        .iter()
        .rev()
        .zip(new.as_bytes()[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point(old, start),
        old_end_position: point(old, old_end),
        new_end_position: point(new, new_end),
    }
}
fn point(text: &str, offset: usize) -> Point {
    let head = &text[..offset.min(text.len())];
    let row = head.bytes().filter(|&b| b == b'\n').count();
    let column = head
        .rfind('\n')
        .map(|i| head.len() - i - 1)
        .unwrap_or(head.len());
    Point { row, column }
}
fn position(text: &str, offset: usize) -> Position {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let line = head.bytes().filter(|&b| b == b'\n').count();
    let start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    Position {
        line: line as u32,
        character: head[start..].encode_utf16().count() as u32,
    }
}
fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}
fn name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .map(|n| text(n, source).to_owned())
}
fn last_name(value: &str) -> Option<String> {
    static IDENT: OnceLock<regex::Regex> = OnceLock::new();
    IDENT
        .get_or_init(|| regex::Regex::new(r"[\p{L}_][\p{L}\p{N}_]*").unwrap())
        .find_iter(value)
        .last()
        .map(|m| m.as_str().into())
}
impl Extract<'_> {
    fn add(
        &mut self,
        kind: &str,
        name: &str,
        start: usize,
        end: usize,
        scope: &str,
        attributes: Value,
    ) -> Option<String> {
        if self.redactor.redact_text(name) != name {
            self.out.redacted += 1;
            return None;
        }
        let qualified = if scope.is_empty() {
            name.into()
        } else {
            format!("{scope}::{name}")
        };
        let id = if kind == "file" {
            self.file.clone()
        } else {
            format!(
                "node_{}",
                hash(format!("{}\0{kind}\0{qualified}\0{start}", self.file).as_bytes())
            )
        };
        if !self.out.nodes.iter().any(|n| n.id == id) {
            self.out.nodes.push(GraphNode {
                id: id.clone(),
                path: Some(self.path.into()),
                kind: kind.into(),
                name: name.into(),
                qualified_name: qualified,
                start_byte: start,
                end_byte: end,
                range: Range {
                    start: position(self.source, start),
                    end: position(self.source, end),
                },
                attributes,
            });
        }
        Some(id)
    }
    fn edge(&mut self, source: &str, target: &str, kind: &str, resolution: &str) {
        let id = format!(
            "edge_{}",
            hash(format!("{source}\0{target}\0{kind}").as_bytes())
        );
        if !self.out.edges.iter().any(|e| e.id == id) {
            self.out.edges.push(GraphEdge {
                id,
                source_id: source.into(),
                destination_id: target.into(),
                kind: kind.into(),
                resolution: resolution.into(),
            });
        }
    }
    fn reference(
        &mut self,
        source: &str,
        target: &str,
        path: Option<String>,
        kind: &str,
        offset: usize,
    ) {
        if self.redactor.redact_text(target) != target
            || path
                .as_ref()
                .is_some_and(|p| self.redactor.redact_text(p) != *p)
        {
            self.out.redacted += 1;
            return;
        }
        let id = format!(
            "ref_{}",
            hash(format!("{source}\0{kind}\0{target}\0{path:?}\0{offset}").as_bytes())
        );
        if !self.out.references.iter().any(|r| r.id == id) {
            self.out.references.push(Reference {
                id,
                source: source.into(),
                kind: kind.into(),
                target: target.into(),
                path,
                offset,
            });
        }
    }
    fn walk(
        &mut self,
        node: Node<'_>,
        owner: &str,
        scope: &str,
        in_test: bool,
        depth: usize,
    ) -> Result<()> {
        if depth > 512 || self.out.nodes.len() > 100_000 {
            return Err(invalid("source syntax graph exceeds traversal budget"));
        }
        if node.is_error() || node.is_missing() {
            self.out.issues.push(SyntaxIssue {
                kind: node.kind().into(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                range: Range {
                    start: position(self.source, node.start_byte()),
                    end: position(self.source, node.end_byte()),
                },
                message: if node.is_missing() {
                    format!("missing {}", node.kind())
                } else {
                    "syntax error".into()
                },
            });
        }
        let mut current = owner.to_owned();
        let mut qualified = scope.to_owned();
        let mut test = in_test;
        let kind = node.kind();
        let declared = name(node, self.source);
        let definition = match kind {
            "function_item"
            | "function_definition"
            | "function_declaration"
            | "generator_function_declaration" => Some("function"),
            "method_definition" | "method_signature" => Some("method"),
            "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "class_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration" => Some("type"),
            "mod_item" | "module" | "internal_module" => Some("module"),
            "variable_declarator"
                if node.child_by_field_name("value").is_some_and(|n| {
                    matches!(
                        n.kind(),
                        "arrow_function" | "function_expression" | "generator_function"
                    )
                }) =>
            {
                Some("function")
            }
            _ => None,
        };
        if let (Some(kind), Some(declared)) = (definition, declared) {
            let attribute_test = node.prev_named_sibling().is_some_and(|n| {
                n.kind() == "attribute_item" && text(n, self.source).contains("test")
            });
            test = declared.starts_with("test_") || attribute_test || in_test;
            let parent_impl = node.parent().is_some_and(|n| {
                matches!(n.kind(), "declaration_list" | "class_body")
                    && n.parent().is_some_and(|p| {
                        matches!(
                            p.kind(),
                            "impl_item" | "trait_item" | "class_declaration" | "class_definition"
                        )
                    })
            });
            let kind = if test && matches!(kind, "function" | "method") {
                "test"
            } else if parent_impl && kind == "function" {
                "method"
            } else {
                kind
            };
            if let Some(id) = self.add(
                kind,
                &declared,
                node.start_byte(),
                node.end_byte(),
                scope,
                json!({"language":self.language.id()}),
            ) {
                self.edge(owner, &id, "defines", "syntactic");
                current = id;
                qualified = format!("{scope}::{declared}");
            }
        }
        if kind == "impl_item"
            && let Some(ty) = node.child_by_field_name("type")
        {
            qualified = format!("{scope}::{}", text(ty, self.source));
        }
        if matches!(kind, "call_expression" | "call" | "new_expression") {
            let function = node
                .child_by_field_name("function")
                .or_else(|| node.child_by_field_name("constructor"));
            if let Some(function) = function
                && let Some(target) = last_name(text(function, self.source))
            {
                if matches!(target.as_str(), "test" | "it")
                    && matches!(
                        self.language,
                        Language::JavaScript | Language::TypeScript | Language::Tsx
                    )
                {
                    let title = node
                        .child_by_field_name("arguments")
                        .and_then(|n| n.named_child(0))
                        .map(|n| text(n, self.source).trim_matches(['\'', '"']).to_owned())
                        .unwrap_or_else(|| format!("test@{}", node.start_byte()));
                    if let Some(id) = self.add(
                        "test",
                        &title,
                        node.start_byte(),
                        node.end_byte(),
                        scope,
                        json!({"framework_call":target}),
                    ) {
                        self.edge(owner, &id, "defines", "syntactic");
                        current = id;
                        test = true;
                    }
                }
                self.reference(&current, &target, None, "call", node.start_byte());
                if test {
                    self.reference(&current, &target, None, "tests", node.start_byte());
                }
            }
        }
        if (matches!(
            kind,
            "use_declaration" | "import_statement" | "import_from_statement" | "import_declaration"
        ) || (kind == "mod_item" && node.child_by_field_name("body").is_none()))
            && let Some(module) = module_path(node, self.source, self.language)
        {
            let base = if self.language == Language::Rust && module.starts_with("crate::") {
                {
                    let prefix = self
                        .path
                        .rfind("/src/")
                        .map(|at| format!("{}/src/", &self.path[..at]))
                        .unwrap_or_else(|| {
                            if self.path.starts_with("src/") || self.path.starts_with("tests/") {
                                "src/".into()
                            } else {
                                String::new()
                            }
                        });
                    format!(
                        "{prefix}{}",
                        module.trim_start_matches("crate::").replace("::", "/")
                    )
                }
            } else if self.language == Language::Python {
                module.replace('.', "/")
            } else {
                module.clone()
            };
            let base = if base.starts_with('.')
                || self.language == Language::Rust && !module.starts_with("crate::")
            {
                join(self.path, &base)
            } else {
                Some(base)
            };
            if let Some(base) = base {
                self.reference(
                    &self.file.clone(),
                    "",
                    Some(base),
                    "import",
                    node.start_byte(),
                );
            }
        }
        if matches!(kind, "identifier" | "type_identifier" | "field_identifier")
            && node.named_child_count() == 0
        {
            let declaration = node
                .parent()
                .and_then(|p| p.child_by_field_name("name"))
                .is_some_and(|name| name.id() == node.id());
            if !declaration {
                let target = text(node, self.source);
                if !target.is_empty() {
                    self.reference(&current, target, None, "reference", node.start_byte());
                }
            }
        }
        if self.language == Language::Markdown && matches!(kind, "atx_heading" | "setext_heading") {
            let title = text(node, self.source)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('#')
                .trim()
                .to_owned();
            let slug = title
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-");
            if let Some(section) = self.add(
                "section",
                &title,
                node.start_byte(),
                node.end_byte(),
                scope,
                json!({"slug":slug}),
            ) {
                self.edge(owner, &section, "defines", "syntactic");
                current = section;
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, &current, &qualified, test, depth + 1)?;
        }
        Ok(())
    }
    fn markdown_inline(&mut self, node: Node<'_>, owner: &str, depth: usize) -> Result<()> {
        if depth > 512 {
            return Err(invalid("markdown syntax exceeds traversal budget"));
        }
        if node.kind() == "link_destination" {
            let value = text(node, self.source).trim_matches(['<', '>']);
            let (path, fragment) = value.split_once('#').unwrap_or((value, ""));
            if !path.contains("://") && !path.starts_with("mailto:") {
                let target = if path.is_empty() {
                    Some(self.path.into())
                } else {
                    join(self.path, path)
                };
                if let Some(path) = target {
                    self.reference(owner, fragment, Some(path), "cites", node.start_byte());
                }
            }
        }
        if node.kind() == "code_span" {
            let value = text(node, self.source).trim_matches('`').trim();
            if !value.contains(char::is_whitespace) && !value.is_empty() {
                self.reference(owner, value, None, "cites", node.start_byte());
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.markdown_inline(child, owner, depth + 1)?;
        }
        Ok(())
    }
}
fn module_path(node: Node<'_>, source: &str, language: Language) -> Option<String> {
    let value = text(node, source);
    if language == Language::Rust {
        let value = node
            .child_by_field_name("argument")
            .map(|n| text(n, source))
            .unwrap_or(value);
        let value = value
            .trim()
            .trim_start_matches("pub ")
            .trim_start_matches("use ")
            .trim_start_matches("mod ")
            .trim_end_matches(';')
            .trim();
        let value = value.split('{').next()?.trim_end_matches("::");
        let mut parts = value.split("::").collect::<Vec<_>>();
        if node.kind() == "use_declaration" && parts.len() > 2 && !text(node, source).contains('{')
        {
            parts.pop();
        }
        return Some(parts.join("::"));
    }
    if language == Language::Python {
        let value = value.trim();
        if let Some(value) = value.strip_prefix("from ") {
            return Some(value.split_whitespace().next()?.into());
        }
        return value
            .strip_prefix("import ")?
            .split([',', ' '])
            .next()
            .map(str::to_owned);
    }
    static QUOTED: OnceLock<regex::Regex> = OnceLock::new();
    QUOTED
        .get_or_init(|| regex::Regex::new(r#"["']([^"']+)["']"#).unwrap())
        .captures(value)
        .map(|c| c[1].to_owned())
}
fn join(source_path: &str, target: &str) -> Option<String> {
    let mut parts = source_path.split('/').collect::<Vec<_>>();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}
