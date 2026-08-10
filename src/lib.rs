//! XML parser plugin - full-parse mode.
//!
//! Handles `.xml`, `.xsd`, `.xslt`, `.svg` files.
//! Parses source with tree-sitter-xml directly.
//!
//! The parser normalises semantically-equivalent forms so that they produce
//! identical structural hashes:
//!
//! - **CDATA ↔ entity-escaped text**: `<![CDATA[a & b]]>` and `a &amp; b` both
//!   produce the same decoded `CharData` text content.
//! - **Namespace prefixes**: `<a:item xmlns:a="http://x">` and
//!   `<item xmlns="http://x">` both resolve to the expanded QName
//!   `{http://x}item`.
//! - **Attribute order**: XML attribute order is semantically insignificant per
//!   the XML Information Set specification; attributes are sorted by name
//!   before hashing.

use std::collections::HashMap;

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct XmlParser;

/// Nodes that carry no semantic information and should be dropped.
/// tree-sitter-xml uses `Comment` for `<!-- ... -->`.
const TRIVIA: &[&str] = &["Comment", "PI"];

/// Semantic node types matching tree-sitter-xml 0.7.x CamelCase naming.
const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "document",
    // Structure
    "element",
    "STag",
    "ETag",
    "EmptyElemTag",
    // Attributes
    "Attribute",
    "Name",
    "AttValue",
    // Content
    "CharData",
    // Declarations
    "XMLDecl",
    "doctypedecl",
    "prolog",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

// ---------------------------------------------------------------------------
// CST normalisation
// ---------------------------------------------------------------------------

/// Decode standard XML entities and numeric character references to their
/// canonical character values.
fn decode_xml_entities(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i + 1..].find(';') {
                let entity = &s[i + 1..i + 1 + semi];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => {
                        if let Some(hex) = entity.strip_prefix("#x") {
                            u32::from_str_radix(hex, 16)
                                .ok()
                                .and_then(char::from_u32)
                        } else if let Some(dec) = entity.strip_prefix('#') {
                            dec.parse::<u32>().ok().and_then(char::from_u32)
                        } else {
                            None
                        }
                    }
                };
                if let Some(ch) = decoded {
                    result.push(ch);
                    i += 1 + semi + 1;
                    continue;
                }
            }
        }
        // Safe because we're iterating over valid UTF-8 and ASCII boundaries.
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        let chunk = std::str::from_utf8(&bytes[i..end]).unwrap_or("\u{FFFD}");
        result.push_str(chunk);
        i = end;
    }
    result
}

fn utf8_char_len(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte >> 5 == 0b110 {
        2
    } else if byte >> 4 == 0b1110 {
        3
    } else if byte >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Extract the text content from a `CDSect` node (the `CData` child text).
fn cdata_text(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "CData" {
            return Some(child.text_or_empty().to_string());
        }
    }
    None
}

/// Decode an `EntityRef` node to its character value.
fn entity_ref_text(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "Name" {
            let name = child.text_or_empty();
            return Some(decode_xml_entities(&format!("&{};", name)));
        }
    }
    None
}

/// Merge consecutive text-level content children (`CharData`, `CDSect`,
/// `EntityRef`) into a single `CharData` node with decoded text so that
/// `<![CDATA[a & b]]>` and `a &amp; b` produce identical content.
fn normalize_content_children(children: &[CstNode]) -> Vec<CstNode> {
    let mut result: Vec<CstNode> = Vec::new();
    let mut merged_text = String::new();
    let mut merged_start: Option<(u32, u32)> = None;
    let mut merged_end: (u32, u32) = (0, 0);

    let text_node_types = ["CharData", "CDSect", "EntityRef"];

    for child in children {
        if text_node_types.contains(&child.node_type.as_str()) {
            let decoded = if child.node_type == "CDSect" {
                cdata_text(child).unwrap_or_default()
            } else if child.node_type == "EntityRef" {
                entity_ref_text(child).unwrap_or_default()
            } else {
                decode_xml_entities(child.text_or_empty())
            };
            if merged_text.is_empty() {
                merged_start = Some((child.start_line, child.start_col));
            }
            merged_text.push_str(&decoded);
            merged_end = (child.end_line, child.end_col);
        } else {
            if !merged_text.is_empty() {
                result.push(CstNode {
                    node_type: "CharData".to_string(),
                    named: true,
                    text: Some(merged_text.clone()),
                    start_line: merged_start.unwrap().0,
                    start_col: merged_start.unwrap().1,
                    end_line: merged_end.0,
                    end_col: merged_end.1,
                    children: Vec::new(),
                });
                merged_text.clear();
            }
            result.push(child.clone());
        }
    }
    if !merged_text.is_empty() {
        result.push(CstNode {
            node_type: "CharData".to_string(),
            named: true,
            text: Some(merged_text),
            start_line: merged_start.unwrap().0,
            start_col: merged_start.unwrap().1,
            end_line: merged_end.0,
            end_col: merged_end.1,
            children: Vec::new(),
        });
    }
    result
}

/// Extract namespace declarations from a tag's `Attribute` children.
/// Returns a map of prefix → URI. The empty-string prefix represents the
/// default namespace (`xmlns="..."`).
fn extract_namespace_decls(children: &[CstNode]) -> HashMap<String, String> {
    let mut decls = HashMap::new();
    for child in children {
        if child.node_type != "Attribute" {
            continue;
        }
        let mut name = "";
        let mut value = "";
        for attr_child in &child.children {
            if attr_child.node_type == "Name" {
                name = attr_child.text_or_empty();
            } else if attr_child.node_type == "AttValue" {
                value = attr_child.text_or_empty().trim_matches('"');
            }
        }
        if let Some(prefix) = name.strip_prefix("xmlns:") {
            decls.insert(prefix.to_string(), value.to_string());
        } else if name == "xmlns" {
            decls.insert(String::new(), value.to_string());
        }
    }
    decls
}

/// Resolve a prefixed QName (e.g. `a:item`) to its expanded form
/// (`{http://x}item`) using the namespace context. Returns `None` if the
/// prefix has no declaration (conservative: leave unmodified).
fn resolve_qname(name: &str, ns_context: &HashMap<String, String>) -> Option<String> {
    if let Some((prefix, local)) = name.split_once(':') {
        if let Some(uri) = ns_context.get(prefix) {
            return Some(format!("{{{}}}{}", uri, local));
        }
    } else if let Some(uri) = ns_context.get("") {
        if !uri.is_empty() {
            return Some(format!("{{{}}}{}", uri, name));
        }
    }
    None
}

/// Recursively normalise the CST tree, carrying namespace context for QName
/// resolution.
fn normalize_cst_with_ns(
    node: &CstNode,
    ns_context: &HashMap<String, String>,
) -> CstNode {
    let mut normalized = node.clone();

    match normalized.node_type.as_str() {
        "element" => {
            // Phase 1: extract namespace declarations from the start tag
            // before recursing, so children inherit the correct context.
            let mut child_ns = ns_context.clone();
            for child in &normalized.children {
                if child.node_type == "STag" || child.node_type == "EmptyElemTag" {
                    let decls = extract_namespace_decls(&child.children);
                    child_ns.extend(decls);
                    break;
                }
            }

            normalized.children = normalized
                .children
                .iter()
                .map(|c| normalize_cst_with_ns(c, &child_ns))
                .collect();

            // Sort attributes by name within each tag so attribute order is
            // representation-invariant per the XML Information Set.
            for child in &mut normalized.children {
                if child.node_type == "STag" || child.node_type == "EmptyElemTag" {
                    sort_tag_attributes(child);
                }
            }
        }
        "STag" | "ETag" | "EmptyElemTag" => {
            // Resolve the tag Name to its expanded QName.
            for child in &mut normalized.children {
                if child.node_type == "Name" {
                    let raw = child.text_or_empty().to_string();
                    if let Some(expanded) = resolve_qname(&raw, ns_context) {
                        child.text = Some(expanded);
                    }
                }
            }
            // Recurse and sort attributes (for STag/EmptyElemTag).
            normalized.children = normalized
                .children
                .iter()
                .map(|c| normalize_cst_with_ns(c, ns_context))
                .collect();
            if normalized.node_type == "STag" || normalized.node_type == "EmptyElemTag" {
                sort_tag_attributes(&mut normalized);
            }
        }
        "content" => {
            // Recurse children first, then merge text-level content.
            normalized.children = normalized
                .children
                .iter()
                .map(|c| normalize_cst_with_ns(c, ns_context))
                .collect();
            normalized.children = normalize_content_children(&normalized.children);
        }
        _ => {
            normalized.children = normalized
                .children
                .iter()
                .map(|c| normalize_cst_with_ns(c, ns_context))
                .collect();
        }
    }

    normalized
}

/// Sort `Attribute` children within a tag by their Name text and strip
/// consumed `xmlns` / `xmlns:*` declaration attributes so that namespace
/// reordering does not produce a diff.
fn sort_tag_attributes(tag: &mut CstNode) {
    let mut attrs: Vec<CstNode> = Vec::new();
    let mut others: Vec<CstNode> = Vec::new();

    for child in tag.children.drain(..) {
        if child.node_type == "Attribute" {
            let name = child
                .children
                .iter()
                .find(|c| c.node_type == "Name")
                .map(|c| c.text_or_empty().to_string())
                .unwrap_or_default();
            // Strip consumed namespace declarations so they don't cause
            // hash mismatches between prefixed and default-namespace forms.
            if name == "xmlns" || name.starts_with("xmlns:") {
                continue;
            }
            attrs.push(child);
        } else {
            others.push(child);
        }
    }

    attrs.sort_by(|a, b| {
        let name_a = a
            .children
            .iter()
            .find(|c| c.node_type == "Name")
            .map(|c| c.text_or_empty())
            .unwrap_or("");
        let name_b = b
            .children
            .iter()
            .find(|c| c.node_type == "Name")
            .map(|c| c.text_or_empty())
            .unwrap_or("");
        name_a.cmp(name_b)
    });

    tag.children = others;
    tag.children.append(&mut attrs);
}

/// Top-level normalisation entry point. Starts with an empty namespace
/// context (no declarations outside the document element).
fn normalize_cst(node: &CstNode) -> CstNode {
    let ns_context = HashMap::new();
    normalize_cst_with_ns(node, &ns_context)
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        // #46: the XML declaration's version/encoding are review content — with the bare
        // kind-name label, an edit inside <?xml version="1.0"?> hashed style-only.
        "XMLDecl" => {
            fn collect(n: &CstNode, out: &mut Vec<String>) {
                if n.is_leaf() {
                    let t = n.text_or_empty().trim();
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
                for c in &n.children {
                    collect(c, out);
                }
            }
            let mut parts = Vec::new();
            collect(node, &mut parts);
            if parts.is_empty() {
                node.node_type.clone()
            } else {
                parts.join(" ").chars().take(120).collect()
            }
        }
        "element" => tag_name_of(node).unwrap_or_else(|| node.node_type.clone()),
        "Attribute" => {
            let name = attribute_name_of(node).unwrap_or_default();
            let value = attribute_value_of(node).unwrap_or_default();
            if value.is_empty() || value == name {
                name
            } else {
                format!("{}={}", name, value)
            }
        }
        _ => node.node_type.clone(),
    }
}

/// Extract the tag name from an element's STag/ETag/EmptyElemTag child.
fn tag_name_of(element: &CstNode) -> Option<String> {
    for child in &element.children {
        if matches!(child.node_type.as_str(), "STag" | "ETag" | "EmptyElemTag") {
            for gc in &child.children {
                if gc.node_type == "Name" {
                    return Some(gc.text_or_empty().to_string());
                }
            }
        }
    }
    None
}

/// Extract the attribute value from an Attribute CST node.
fn attribute_value_of(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "AttValue" {
            return Some(child.text_or_empty().trim_matches('"').to_string());
        }
    }
    None
}

/// Extract the name from an Attribute CST node.
fn attribute_name_of(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "Name" {
            return Some(child.text_or_empty().to_string());
        }
    }
    None
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    memo: &mut HashMap<usize, String>,
) -> Option<SemanticNode> {
    if TRIVIA.contains(&node.node_type.as_str()) {
        return None;
    }

    // Elements get special handling: flatten STag/ETag/content wrappers so
    // that attributes and text content become direct children.
    if node.node_type == "element" {
        return convert_element(node, id_prefix, memo);
    }

    // Non-element nodes: only keep if semantic.
    if !is_semantic(&node.node_type) {
        return None;
    }

    let children: Vec<SemanticNode> = node
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| convert(c, &format!("{}.{}", id_prefix, i), memo))
        .collect();

    let hash = structural_hash_with_memo(node, memo);
    Some(
        SemanticNodeBuilder::new(
            id_prefix,
            &node.node_type,
            label_for(node),
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        )
        .children(children)
        .build(),
    )
}

/// Convert an XML element into a clean semantic node where attributes and
/// text content are direct children, without STag/ETag/content wrappers.
fn convert_element(
    node: &CstNode,
    id_prefix: &str,
    memo: &mut HashMap<usize, String>,
) -> Option<SemanticNode> {
    let mut child_idx = 0usize;
    let mut children: Vec<SemanticNode> = Vec::new();

    for child in &node.children {
        match child.node_type.as_str() {
            "STag" | "EmptyElemTag" => {
                // Promote Attribute children from the opening tag.
                for tag_child in &child.children {
                    if tag_child.node_type == "Attribute" {
                        if let Some(sem) = convert_attribute_node(
                            tag_child,
                            &format!("{}.{}", id_prefix, child_idx),
                            memo,
                        ) {
                            children.push(sem);
                            child_idx += 1;
                        }
                    }
                }
            }
            "ETag" => {
                // End tags carry no content; skip entirely.
            }
            "content" => {
                // Promote child elements only; text content (CharData) is
                // captured in the element's structural hash but intentionally
                // excluded as a separate semantic node so that text-value
                // changes surface as element MODIFICATIONs (matching the HTML
                // parser's approach). The host-side enrichment layer adds
                // synthetic text children for review display.
                for content_child in &child.children {
                    if content_child.node_type == "element" {
                        if let Some(sem) = convert(
                            content_child,
                            &format!("{}.{}", id_prefix, child_idx),
                            memo,
                        ) {
                            children.push(sem);
                            child_idx += 1;
                        }
                    }
                }
            }
            _ => {
                // Direct child element or other semantic node.
                if let Some(sem) = convert(child, &format!("{}.{}", id_prefix, child_idx), memo)
                {
                    children.push(sem);
                    child_idx += 1;
                }
            }
        }
    }

    let hash = structural_hash_with_memo(node, memo);
    let tag = tag_name_of(node).unwrap_or_else(|| "element".to_string());
    Some(
        SemanticNodeBuilder::new(
            id_prefix,
            "element",
            &tag,
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        )
        .children(children)
        .build(),
    )
}

/// Convert an Attribute CST node to a semantic node with a clean label.
fn convert_attribute_node(
    node: &CstNode,
    id_prefix: &str,
    memo: &mut HashMap<usize, String>,
) -> Option<SemanticNode> {
    let hash = structural_hash_with_memo(node, memo);
    Some(
        SemanticNodeBuilder::new(
            id_prefix,
            "attribute",
            &label_for(node),
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        )
        .children(Vec::new())
        .build(),
    )
}



use intentumdiff_plugin_sdk::ts_convert::node_to_cst;

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_xml::LANGUAGE_XML.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load XML grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let root = normalize_cst(&root);
    let mut memo: HashMap<usize, String> = HashMap::new();
    let sem = match convert(&root, "0", &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for XmlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "xml".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".xml")
            || lower.ends_with(".xsd")
            || lower.ends_with(".xslt")
            || lower.ends_with(".xsl")
            || lower.ends_with(".svg")
            || lower.ends_with(".plist")
            || lower.ends_with(".csproj")
            || lower.ends_with(".vbproj")
            || lower.ends_with(".fsproj")
            || lower.ends_with(".props")
            || lower.ends_with(".targets")
        {
            return "xml".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n  <name>MyApp</name>\n  <version>1.0</version>\n  <server>\n    <host>localhost</host>\n    <port>8080</port>\n  </server>\n</config>\n".to_string(),
            new: "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n  <name>MyApp</name>\n  <version>2.0</version>\n  <server>\n    <host>0.0.0.0</host>\n    <port>8080</port>\n    <timeout>30</timeout>\n  </server>\n  <database>\n    <host>db.example.com</host>\n    <port>5432</port>\n    <name>mydb</name>\n  </database>\n</config>\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["xml".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(XmlParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!XmlParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = XmlParser::grammar_id();
        let ids = XmlParser::language_ids();
        assert!(ids.contains(&gid));
    }

    #[test]
    fn detect_language_known_ext() {
        assert_eq!(
            XmlParser::detect_language("test.xml".to_string(), "".to_string()).as_str(),
            "xml"
        );
    }

    #[test]
    fn detect_language_unknown_ext() {
        assert_eq!(
            XmlParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string())
                .as_str(),
            ""
        );
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    // --- Normalisation tests ---

    fn hash_of(source: &str) -> String {
        let root = parse_source(source).unwrap();
        let normalized = normalize_cst(&root);
        structural_hash_with_memo(&normalized, &mut HashMap::new())
    }

    #[test]
    fn cdata_and_entity_escaped_text_produce_same_hash() {
        let cdata = hash_of("<msg><![CDATA[a & b < c]]></msg>");
        let entity = hash_of("<msg>a &amp; b &lt; c</msg>");
        assert_eq!(cdata, entity, "CDATA and entity-escaped text must match");
    }

    #[test]
    fn namespace_prefix_and_default_produce_same_hash() {
        let prefixed = hash_of(r#"<a:item xmlns:a="http://x">v</a:item>"#);
        let default = hash_of(r#"<item xmlns="http://x">v</item>"#);
        assert_eq!(
            prefixed, default,
            "prefixed and default-namespace elements must match"
        );
    }

    #[test]
    fn attribute_reorder_produces_same_hash() {
        let order_a = hash_of(r#"<elem a="1" b="2" c="3"/>"#);
        let order_b = hash_of(r#"<elem c="3" a="1" b="2"/>"#);
        assert_eq!(order_a, order_b, "attribute reordering must not change hash");
    }

    #[test]
    fn different_attribute_values_produce_different_hash() {
        let v1 = hash_of(r#"<elem x="1"/>"#);
        let v2 = hash_of(r#"<elem x="2"/>"#);
        assert_ne!(v1, v2);
    }

    #[test]
    fn undeclared_namespace_prefix_is_not_resolved() {
        let root = parse_source(r#"<a:item>v</a:item>"#).unwrap();
        let normalized = normalize_cst(&root);
        // Walk to find the STag Name and check it was NOT expanded
        let name_text = normalized
            .walk()
            .find(|n| n.node_type == "STag")
            .and_then(|tag| tag.children.iter().find(|c| c.node_type == "Name"))
            .map(|n| n.text_or_empty().to_string())
            .unwrap_or_default();
        assert_eq!(name_text, "a:item", "undeclared prefixes must stay as-is");
    }

    #[test]
    fn decode_entities_roundtrip() {
        assert_eq!(decode_xml_entities("a &amp; b &lt; c"), "a & b < c");
        assert_eq!(decode_xml_entities("&quot;hi&quot;"), "\"hi\"");
        assert_eq!(decode_xml_entities("&#65;"), "A");
        assert_eq!(decode_xml_entities("&#x42;"), "B");
    }

    #[test]
    fn semantic_tree_has_element_children_after_fix() {
        let out = process_impl(r#"<root><child>text</child></root>"#);
        let tree: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(tree.get("children").is_some(), "root must have children");
    }
}
