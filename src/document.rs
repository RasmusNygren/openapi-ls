use lsp_types::{Position, Range};
use tree_sitter::{Node, Parser, Tree};

use crate::Result;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Json,
    Yaml,
}

impl Format {
    pub(crate) fn for_path(path: &str) -> Self {
        if path.ends_with(".json") {
            Self::Json
        } else {
            Self::Yaml
        }
    }
}

pub(crate) struct Reference {
    pub(crate) value: String,
    pub(crate) range: Range,
}

#[derive(Clone)]
pub(crate) struct Document {
    text: String,
    tree: Tree,
    lines: Vec<usize>,
    pub(crate) format: Format,
}

impl Document {
    pub(crate) fn new(text: String, format: Format) -> Result<Self> {
        let mut parser = Parser::new();
        let language = match format {
            Format::Json => tree_sitter_json::LANGUAGE,
            Format::Yaml => tree_sitter_yaml::LANGUAGE,
        };
        parser.set_language(&language.into())?;
        // ponytail: reparse whole documents; reuse trees if editing large specs becomes slow.
        let tree = parser.parse(&text, None).ok_or("parsing was cancelled")?;
        let lines = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        Ok(Self {
            text,
            tree,
            lines,
            format,
        })
    }

    /// LSP uses UTF-16 code units, while Tree-sitter uses UTF-8 byte offsets.
    pub(crate) fn offset(&self, position: Position) -> Option<usize> {
        let start = *self.lines.get(position.line as usize)?;
        let end = self
            .lines
            .get(position.line as usize + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line = self.text[start..end].trim_end_matches(['\r', '\n']);
        let mut units = 0;
        for (byte, ch) in line.char_indices() {
            if units == position.character {
                return Some(start + byte);
            }
            units += ch.len_utf16() as u32;
            if units > position.character {
                return None; // A position inside a surrogate pair is invalid.
            }
        }
        // LSP positions beyond the line's length clamp to its end.
        Some(start + line.len())
    }

    fn position(&self, byte: usize) -> Position {
        let line = self.lines.partition_point(|&start| start <= byte) - 1;
        let character = self.text[self.lines[line]..byte].encode_utf16().count();
        Position::new(line as u32, character as u32)
    }

    fn range(&self, node: Node<'_>) -> Range {
        Range::new(
            self.position(node.start_byte()),
            self.position(node.end_byte()),
        )
    }

    fn scalar(&self, node: Node<'_>) -> Option<String> {
        let node = content(node)?;
        if node.has_error() || node.is_missing() {
            return None;
        }
        let source = &self.text[node.byte_range()];
        match (self.format, node.kind()) {
            (Format::Json, "string") => serde_json::from_str(source).ok(),
            (Format::Yaml, "plain_scalar" | "single_quote_scalar" | "double_quote_scalar") => {
                serde_saphyr::from_str(source).ok()
            }
            _ => None,
        }
    }

    pub(crate) fn reference_at(&self, position: Position) -> Option<Reference> {
        let root = content(self.tree.root_node())?;
        let offset = self.offset(position)?;
        let mut node = root.descendant_for_byte_range(offset, offset)?;
        let mut reference = None;
        loop {
            if is_pair(node) {
                let key = node.child_by_field_name("key")?;
                let value = node.child_by_field_name("value")?;
                if self.scalar(key).as_deref() == Some("$ref")
                    && (value.byte_range().contains(&offset) || key.byte_range().contains(&offset))
                {
                    reference = self.scalar(value).map(|value_text| Reference {
                        value: value_text,
                        range: self.range(value),
                    });
                }
            }
            // Schema resource scopes need a separate resolver. Don't return a wrong target.
            if is_mapping(node) && self.member(node, "$id").is_some() {
                return None;
            }
            node = match node.parent() {
                Some(parent) => parent,
                None => return reference,
            };
        }
    }

    pub(crate) fn references(&self) -> Vec<Reference> {
        let Some(root) = content(self.tree.root_node()) else {
            return Vec::new();
        };
        let mut references = Vec::new();
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if is_pair(node)
                && let Some(key) = node.child_by_field_name("key")
                && self.scalar(key).as_deref() == Some("$ref")
                && let Some(reference) = self.reference_at(self.position(key.start_byte()))
            {
                references.push(reference);
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return references;
                }
            }
        }
    }

    /// Select a declaration key or array element under the cursor.
    pub(crate) fn symbol_at(&self, position: Position) -> Option<Range> {
        let root = content(self.tree.root_node())?;
        let offset = self.offset(position)?;
        let mut node = root.descendant_for_byte_range(offset, offset)?;
        loop {
            if is_pair(node)
                && let Some(key) = node.child_by_field_name("key")
                && key.byte_range().contains(&offset)
                && !key.has_error()
            {
                return Some(self.range(key));
            }
            let parent = node.parent()?;
            if is_sequence(parent) && !node.is_extra() {
                return Some(self.range(content(node).unwrap_or(node)));
            }
            node = parent;
        }
    }

    pub(crate) fn preview(&self, range: Range) -> Option<String> {
        let root = content(self.tree.root_node())?;
        let start = self.offset(range.start)?;
        let end = self.offset(range.end)?;
        let mut node = root.descendant_for_byte_range(start, end)?;
        while let Some(parent) = node.parent() {
            if is_pair(parent)
                && parent
                    .child_by_field_name("key")
                    .is_some_and(|key| key.byte_range() == (start..end))
            {
                node = parent;
                break;
            }
            if parent.byte_range() != (start..end) {
                break;
            }
            node = parent;
        }
        let indent = " ".repeat(node.start_position().column);
        let source = &self.text[node.byte_range()];
        let mut preview = String::new();
        for (index, line) in source.lines().enumerate() {
            let line = if index == 0 {
                line
            } else {
                line.strip_prefix(&indent).unwrap_or(line)
            };
            if index >= 40 {
                preview.push_str("… (preview truncated)\n");
                break;
            }
            if preview.len() + line.len() > 4000 {
                let remaining = 4000_usize.saturating_sub(preview.len());
                preview.push_str(&line[..line.floor_char_boundary(remaining)]);
                preview.push_str("\n… (preview truncated)\n");
                break;
            }
            preview.push_str(line);
            preview.push('\n');
        }
        Some(preview)
    }

    fn member<'a>(&self, node: Node<'a>, name: &str) -> Option<(Node<'a>, Node<'a>)> {
        let mut cursor = node.walk();
        let mut found = None;
        for pair in node
            .named_children(&mut cursor)
            .filter(|child| is_pair(*child))
        {
            let Some(key) = pair.child_by_field_name("key") else {
                continue;
            };
            if self.scalar(key).as_deref() == Some(name) {
                if found.is_some() {
                    return None; // Duplicate keys have no unambiguous definition.
                }
                found = Some((key, pair.child_by_field_name("value").unwrap_or(key)));
            }
        }
        found
    }

    pub(crate) fn target(&self, pointer: &[String]) -> Option<Range> {
        let mut node = content(self.tree.root_node())?;
        let mut selection = node;
        for token in pointer {
            node = content(node)?;
            if is_mapping(node) {
                let (key, value) = self.member(node, token)?;
                selection = key;
                node = value;
            } else if is_sequence(node) {
                if token.is_empty()
                    || (token.starts_with('0') && token.len() > 1)
                    || !token.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                let mut cursor = node.walk();
                node = node
                    .named_children(&mut cursor)
                    .filter(|child| !child.is_extra())
                    .nth(token.parse::<usize>().ok()?)?;
                selection = content(node).unwrap_or(node);
            } else {
                return None;
            }
        }
        (!selection.has_error() && !selection.is_missing()).then(|| self.range(selection))
    }
}

fn is_pair(node: Node<'_>) -> bool {
    matches!(node.kind(), "pair" | "block_mapping_pair" | "flow_pair")
}

fn is_sequence(node: Node<'_>) -> bool {
    matches!(node.kind(), "array" | "block_sequence" | "flow_sequence")
}

fn is_mapping(node: Node<'_>) -> bool {
    // During incomplete edits, Tree-sitter can retain intact pairs directly under ERROR.
    matches!(
        node.kind(),
        "object" | "block_mapping" | "flow_mapping" | "ERROR"
    )
}

/// Strip grammar wrappers without descending through mappings or sequences.
fn content(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(
        node.kind(),
        "stream" | "document" | "block_node" | "flow_node" | "block_sequence_item"
    ) {
        let mut cursor = node.walk();
        let mut children = node.named_children(&mut cursor).filter(|child| {
            !child.is_extra()
                && !matches!(
                    child.kind(),
                    "anchor" | "tag" | "yaml_directive" | "tag_directive" | "reserved_directive"
                )
        });
        let child = children.next()?;
        if children.next().is_some() {
            return None; // OpenAPI is a single document, not a YAML document stream.
        }
        node = child;
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigates_yaml_and_json_with_decoded_keys_and_arrays() {
        for (format, source) in [
            (
                Format::Yaml,
                "components:\n  schemas:\n    'Pet/name': {}\nitems:\n  - name: first\n$ref: '#/components/schemas/Pet~1name'\n",
            ),
            (
                Format::Yaml,
                "{components: {schemas: {'Pet/name': {}}}, items: [{name: first}], $ref: '#/components/schemas/Pet~1name'}",
            ),
            (
                Format::Json,
                r##"{"components":{"schemas":{"Pet\u002fname":{}}},"items":[{"name":"first"}],"$ref":"#/components/schemas/Pet~1name"}"##,
            ),
        ] {
            let doc = Document::new(source.into(), format).unwrap();
            let reference = doc.position(source.find("#/components").unwrap());
            assert_eq!(
                doc.reference_at(reference)
                    .map(|reference| reference.value)
                    .as_deref(),
                Some("#/components/schemas/Pet~1name")
            );
            let target = doc
                .target(&["components".into(), "schemas".into(), "Pet/name".into()])
                .unwrap();
            assert_eq!(doc.symbol_at(target.start), Some(target));
            assert!(doc.preview(target).unwrap().contains("{}"));
            let references = doc.references();
            assert_eq!(references.len(), 1);
            assert_eq!(references[0].value, "#/components/schemas/Pet~1name");
            assert_eq!(
                doc.reference_at(references[0].range.start).unwrap().value,
                references[0].value
            );
            assert_eq!(
                target.start,
                doc.position(
                    source
                        .find("'Pet/name'")
                        .or_else(|| source.find("\"Pet\\u002fname\""))
                        .unwrap()
                )
            );
            assert!(
                doc.target(&["items".into(), "0".into(), "name".into()])
                    .is_some()
            );
            for index in ["01", "-", "+0", "1", "999999999999999999999999"] {
                assert!(doc.target(&["items".into(), index.into()]).is_none());
            }
        }
    }

    #[test]
    fn positions_handle_unicode_crlf_and_trailing_newline() {
        let doc = Document::new("a😀é\r\nb\n".into(), Format::Yaml).unwrap();
        for (position, offset) in [
            (Position::new(0, 0), 0),
            (Position::new(0, 1), 1),
            (Position::new(0, 3), 5),
            (Position::new(0, 4), 7),
            (Position::new(1, 0), 9),
            (Position::new(2, 0), 11),
        ] {
            assert_eq!(doc.offset(position), Some(offset));
            assert_eq!(doc.position(offset), position);
        }
        assert_eq!(doc.offset(Position::new(0, 99)), Some(7));
        assert_eq!(doc.offset(Position::new(0, 2)), None);
        assert_eq!(doc.offset(Position::new(3, 0)), None);
    }

    #[test]
    fn incomplete_documents_and_unsupported_constructs_are_safe() {
        let source = "Pet: {}\n$ref: '#/Pet'\nbroken: [\n";
        let doc = Document::new(source.into(), Format::Yaml).unwrap();
        assert!(
            doc.target(&["Pet".into()]).is_some(),
            "{}",
            doc.tree.root_node().to_sexp()
        );
        assert_eq!(
            doc.reference_at(doc.position(source.find("#/Pet").unwrap()))
                .map(|reference| reference.value)
                .as_deref(),
            Some("#/Pet")
        );
        for source in ["Pet: {}\nPet: {}\n", "---\nPet: {}\n---\nPet: {}\n"] {
            assert!(
                Document::new(source.into(), Format::Yaml)
                    .unwrap()
                    .target(&["Pet".into()])
                    .is_none()
            );
        }
        let source = "$id: child.json\n$ref: '#/Pet'\nPet: {}\n";
        let doc = Document::new(source.into(), Format::Yaml).unwrap();
        assert!(
            doc.reference_at(doc.position(source.find("#/Pet").unwrap()))
                .is_none()
        );
        assert!(doc.references().is_empty());
    }

    #[test]
    fn hover_previews_preserve_indentation_and_bound_large_unicode_values() {
        let doc = Document::new(
            "schemas:\n  Pet:\n    type: object\n    description: 'A pet'\n".into(),
            Format::Yaml,
        )
        .unwrap();
        let range = doc.target(&["schemas".into(), "Pet".into()]).unwrap();
        assert_eq!(
            doc.preview(range).unwrap(),
            "Pet:\n  type: object\n  description: 'A pet'\n"
        );
        let source = format!("{{\"description\":\"{}\"}}", "😀".repeat(2000));
        let doc = Document::new(source, Format::Json).unwrap();
        let preview = doc.preview(doc.target(&[]).unwrap()).unwrap();
        assert!(preview.starts_with("{\"description\":\"😀"));
        assert!(preview.contains("preview truncated"));
        assert!(preview.len() < 4100);
        let source = format!("Pet:\n{}", "  description: text\n".repeat(60));
        let doc = Document::new(source, Format::Yaml).unwrap();
        let preview = doc.preview(doc.target(&["Pet".into()]).unwrap()).unwrap();
        assert_eq!(preview.lines().count(), 41);
    }
}
