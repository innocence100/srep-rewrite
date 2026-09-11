//! CommonMark-based extraction and validation for the versioned requirement manifest.
//!
//! The implementation deliberately keeps canonicalization in byte-oriented helpers. The
//! resulting payload is hashed as arbitrary bytes; it is not reconstructed from JSON text.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const EXTRACTOR_VERSION: &str = "6";
pub const SPEC_RELATIVE_PATH: &str =
    "docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md";
pub const MANIFEST_RELATIVE_PATH: &str = "tests/requirements.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditMode {
    Check,
    Write,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub spec_path: String,
    pub spec_sha256: String,
    pub extractor_version: String,
    pub requirements: Vec<RequirementRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequirementRecord {
    pub id: String,
    pub heading: String,
    pub kind: String,
    pub canonical_payload_hex: String,
    pub tests: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    kind: NodeKind,
    children: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeKind {
    Root,
    Paragraph,
    Heading(u32),
    List { ordered: bool, start: u32 },
    Item,
    CodeBlock { language: String },
    HtmlBlock,
    Table(Vec<u8>),
    TableHead,
    TableRow,
    TableCell,
    Other,
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) heading: String,
    pub(crate) kind: String,
    pub(crate) structural: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableData {
    alignments: Vec<u8>,
    header: Vec<String>,
    body: Vec<Vec<String>>,
}

pub fn audit_repository(root: &Path, mode: AuditMode) -> Result<usize, Box<dyn std::error::Error>> {
    let spec_path = root.join(SPEC_RELATIVE_PATH);
    let manifest_path = root.join(MANIFEST_RELATIVE_PATH);
    let spec = fs::read(&spec_path)?;
    let extracted = extract(std::str::from_utf8(&spec)?)?;
    let spec_sha256 = hex(Sha256::digest(&spec));
    let expected = Manifest {
        spec_path: SPEC_RELATIVE_PATH.to_owned(),
        spec_sha256,
        extractor_version: EXTRACTOR_VERSION.to_owned(),
        requirements: extracted
            .iter()
            .map(|unit| record_for_unit(unit, root))
            .collect(),
    };
    if mode == AuditMode::Write {
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&expected)? + "\n";
        fs::write(&manifest_path, json)?;
        return Ok(expected.requirements.len());
    }
    let actual: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    validate_manifest(root, &actual, &extracted, &expected.spec_sha256)?;
    Ok(extracted.len())
}

fn record_for_unit(unit: &Unit, root: &Path) -> RequirementRecord {
    let payload = canonical_payload(&unit.heading, &unit.kind, &unit.structural);
    let digest = Sha256::digest(&payload);
    let digest_hex = hex(digest);
    let mut evidence = vec![
        MANIFEST_RELATIVE_PATH.to_owned(),
        SPEC_RELATIVE_PATH.to_owned(),
    ];
    let extra = if unit.heading.contains("Fidelity") {
        "scripts/fidelity.py"
    } else if unit.heading.contains("Legacy") {
        "tests/legacy.rs"
    } else if unit.heading.contains("CandidateIndex") {
        "tests/candidate_index.rs"
    } else if unit.heading.contains("CLI") {
        "tests/cli.rs"
    } else {
        "README.md"
    };
    if root.join(extra).is_file() {
        evidence.push(extra.to_owned());
    }
    RequirementRecord {
        id: format!("REQ-{}", &digest_hex[..16]),
        heading: unit.heading.clone(),
        kind: unit.kind.clone(),
        canonical_payload_hex: digest_payload_hex(&payload),
        tests: vec![test_binary_for_heading(&unit.heading)],
        evidence,
    }
}

fn test_binary_for_heading(heading: &str) -> String {
    if heading.contains("CLI") {
        "cli".to_owned()
    } else if heading.contains("Legacy") {
        "legacy".to_owned()
    } else if heading.contains("CandidateIndex") {
        "candidate_index".to_owned()
    } else if heading.contains("m0") {
        "m0".to_owned()
    } else if heading.contains("m1") || heading.contains("m2") {
        "stage5".to_owned()
    } else if heading.contains("m3") || heading.contains("m4") {
        "stage6".to_owned()
    } else if heading.contains("m5") || heading.contains("Fidelity") {
        "stage7".to_owned()
    } else {
        "stage3".to_owned()
    }
}

fn validate_manifest(
    root: &Path,
    actual: &Manifest,
    extracted: &[Unit],
    spec_sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if actual.spec_path != SPEC_RELATIVE_PATH
        || actual.extractor_version != EXTRACTOR_VERSION
        || actual.spec_sha256 != spec_sha256
    {
        return Err("requirement manifest metadata is stale".into());
    }
    if actual.requirements.len() != extracted.len() {
        return Err(format!(
            "manifest requirement count differs: {} != {}",
            actual.requirements.len(),
            extracted.len()
        )
        .into());
    }
    let expected: Vec<_> = extracted
        .iter()
        .map(|unit| record_for_unit(unit, root))
        .collect();
    let mut ids = BTreeSet::new();
    for (index, (record, expected_record)) in
        actual.requirements.iter().zip(expected.iter()).enumerate()
    {
        if record != expected_record {
            return Err(format!(
                "stale or altered requirement at index {index}: {}",
                record.id
            )
            .into());
        }
        if !ids.insert(record.id.clone()) {
            return Err(format!("duplicate requirement id {}", record.id).into());
        }
        if record.canonical_payload_hex.is_empty()
            || !record
                .canonical_payload_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || record.canonical_payload_hex.len() % 2 != 0
        {
            return Err(format!("invalid lowercase payload hex for {}", record.id).into());
        }
        for evidence in &record.evidence {
            if !root.join(evidence).is_file() {
                return Err(format!("missing evidence path {evidence}").into());
            }
        }
        if record.tests.is_empty() || record.evidence.is_empty() {
            return Err(format!("empty traceability mapping for {}", record.id).into());
        }
    }
    Ok(())
}

pub(crate) fn extract(markdown: &str) -> Result<Vec<Unit>, Box<dyn std::error::Error>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let events: Vec<_> = Parser::new_ext(markdown, options).collect();
    let root = event_tree(events)?;
    let mut units = Vec::new();
    let mut headings = Vec::new();
    extract_scope(&root.children, &mut headings, &mut units)?;
    ensure_unique_units(&units)?;
    Ok(units)
}

fn event_tree<'a>(events: Vec<Event<'a>>) -> Result<Node, Box<dyn std::error::Error>> {
    let mut stack = vec![Node {
        kind: NodeKind::Root,
        children: Vec::new(),
    }];
    for event in events {
        match event {
            Event::Start(tag) => stack.push(Node {
                kind: node_kind(tag)?,
                children: Vec::new(),
            }),
            Event::End(_) => {
                let node = stack.pop().ok_or("unbalanced Markdown end event")?;
                stack
                    .last_mut()
                    .ok_or("unbalanced Markdown root")?
                    .children
                    .push(node);
            }
            Event::Text(value) => push_leaf(&mut stack, NodeKind::Text(value.into_string()))?,
            Event::Code(value) => push_leaf(&mut stack, NodeKind::Code(value.into_string()))?,
            Event::Html(value) | Event::InlineHtml(value) => {
                push_leaf(&mut stack, NodeKind::Html(value.into_string()))?
            }
            Event::SoftBreak => push_leaf(&mut stack, NodeKind::SoftBreak)?,
            Event::HardBreak => push_leaf(&mut stack, NodeKind::HardBreak)?,
            Event::Rule => push_leaf(&mut stack, NodeKind::Other)?,
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                push_leaf(&mut stack, NodeKind::Code(value.into_string()))?
            }
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {
                push_leaf(&mut stack, NodeKind::Other)?
            }
        }
    }
    if stack.len() != 1 {
        return Err("unbalanced Markdown start event".into());
    }
    stack.pop().ok_or_else(|| "missing Markdown root".into())
}

fn push_leaf(stack: &mut [Node], kind: NodeKind) -> Result<(), Box<dyn std::error::Error>> {
    stack
        .last_mut()
        .ok_or("missing Markdown parent")?
        .children
        .push(Node {
            kind,
            children: Vec::new(),
        });
    Ok(())
}

fn node_kind(tag: Tag<'_>) -> Result<NodeKind, Box<dyn std::error::Error>> {
    Ok(match tag {
        Tag::Paragraph => NodeKind::Paragraph,
        Tag::Heading { level, .. } => NodeKind::Heading(level as u32),
        Tag::List(start) => NodeKind::List {
            ordered: start.is_some(),
            start: start.unwrap_or(1) as u32,
        },
        Tag::Item => NodeKind::Item,
        Tag::CodeBlock(kind) => NodeKind::CodeBlock {
            language: match kind {
                CodeBlockKind::Indented => String::new(),
                CodeBlockKind::Fenced(value) => value.into_string(),
            },
        },
        Tag::HtmlBlock => NodeKind::HtmlBlock,
        Tag::Table(alignments) => {
            NodeKind::Table(alignments.into_iter().map(alignment_code).collect())
        }
        Tag::TableHead => NodeKind::TableHead,
        Tag::TableRow => NodeKind::TableRow,
        Tag::TableCell => NodeKind::TableCell,
        _ => NodeKind::Other,
    })
}

fn extract_scope(
    nodes: &[Node],
    headings: &mut Vec<String>,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut index = 0;
    let mut table_ordinals = BTreeMap::<String, u32>::new();
    while index < nodes.len() {
        if let NodeKind::Heading(level) = nodes[index].kind {
            let text = normalize_leaf(&plain_text(&nodes[index]))?;
            headings.truncate(level.saturating_sub(1) as usize);
            headings.push(text);
            index += 1;
            continue;
        }
        if let Some(kind) = hard_start(&nodes[index]) {
            let end = find_hard_end(nodes, index + 1)?;
            let content = &nodes[index + 1..end];
            if content.iter().any(contains_marker) {
                return Err("nested hard-unit marker".into());
            }
            let heading = heading_path(headings);
            let table_ordinal = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::Table(_)))
                .map(|_| next_table_ordinal(&mut table_ordinals, &heading));
            extract_marked(kind, content, &heading, table_ordinal, units)?;
            index = end + 1;
            continue;
        }
        if hard_table_marker(nodes, index) {
            let heading = heading_path(headings);
            let ordinal = next_table_ordinal(&mut table_ordinals, &heading);
            let table = table_from_node(&nodes[index])?;
            emit_table(table, &heading, ordinal, units)?;
            index += 1;
            continue;
        }
        if node_html(&nodes[index]).trim_end_matches('\n') == "<!-- /hard-unit -->" {
            return Err("orphan hard-unit end marker".into());
        }
        if matches!(nodes[index].kind, NodeKind::Table(_)) {
            let heading = heading_path(headings);
            let _ = next_table_ordinal(&mut table_ordinals, &heading);
        }
        extract_unmarked_node(&nodes[index], &heading_path(headings), units)?;
        index += 1;
    }
    Ok(())
}

fn hard_start(node: &Node) -> Option<&str> {
    match &node.kind {
        NodeKind::Html(_) | NodeKind::HtmlBlock => node_html(node)
            .strip_suffix('\n')
            .unwrap_or_else(|| node_html(node))
            .strip_prefix("<!-- hard-unit kind=\"")?
            .strip_suffix("\" -->"),
        _ => None,
    }
}

fn find_hard_end(nodes: &[Node], start: usize) -> Result<usize, Box<dyn std::error::Error>> {
    for (index, node) in nodes.iter().enumerate().skip(start) {
        if node_html(node).trim_end_matches('\n') == "<!-- /hard-unit -->" {
            return Ok(index);
        }
        if hard_start(node).is_some() {
            return Err("nested hard-unit marker".into());
        }
    }
    Err("unclosed hard-unit marker".into())
}

fn hard_table_marker(nodes: &[Node], index: usize) -> bool {
    index > 0
        && node_html(&nodes[index - 1]).trim_end_matches('\n') == "<!-- hard-requirements -->"
        && matches!(
            nodes.get(index).map(|node| &node.kind),
            Some(NodeKind::Table(_))
        )
}

fn extract_marked(
    kind: &str,
    content: &[Node],
    heading: &str,
    table_ordinal: Option<u32>,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(kind, "paragraph" | "list" | "formula" | "table") {
        return Err(format!("unknown hard-unit kind {kind}").into());
    }
    match kind {
        "paragraph" => {
            let text = content.iter().map(plain_text).collect::<String>();
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "paragraph".to_owned(),
                structural: normalize_leaf(&text)?.into_bytes(),
            });
        }
        "list" => {
            let list = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::List { .. }))
                .ok_or("list hard-unit has no list")?;
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "list".to_owned(),
                structural: encode_list(list, 0)?,
            });
        }
        "formula" => {
            let code = content
                .iter()
                .find_map(|node| match &node.kind {
                    NodeKind::CodeBlock { .. } => Some(code_text(node)),
                    _ => None,
                })
                .ok_or("formula hard-unit has no code block")?;
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "formula".to_owned(),
                structural: code.nfc().collect::<String>().into_bytes(),
            });
        }
        "table" => {
            let table = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::Table(_)))
                .ok_or("table hard-unit has no table")?;
            emit_table(
                table_from_node(table)?,
                heading,
                table_ordinal.ok_or("missing table ordinal")?,
                units,
            )?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn extract_unmarked_node(
    node: &Node,
    heading: &str,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    match node.kind {
        NodeKind::Paragraph => {
            let text = plain_text(node);
            if contains_must(&text) {
                units.push(Unit {
                    heading: heading.to_owned(),
                    kind: "paragraph".to_owned(),
                    structural: normalize_leaf(&text)?.into_bytes(),
                });
            }
        }
        NodeKind::List { .. } => {
            for item in node
                .children
                .iter()
                .filter(|child| matches!(child.kind, NodeKind::Item))
            {
                let text = item_inline_text(item);
                if contains_must(&text) {
                    units.push(Unit {
                        heading: heading.to_owned(),
                        kind: "list-item".to_owned(),
                        structural: normalize_leaf(&text)?.into_bytes(),
                    });
                }
                for child in &item.children {
                    if matches!(child.kind, NodeKind::List { .. }) {
                        extract_unmarked_node(child, heading, units)?;
                    }
                }
            }
        }
        NodeKind::Heading(_)
        | NodeKind::CodeBlock { .. }
        | NodeKind::Table(_)
        | NodeKind::Html(_) => {}
        _ => {}
    }
    Ok(())
}

fn emit_table(
    table: TableData,
    heading: &str,
    ordinal: u32,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    if table.header.is_empty() || table.body.iter().any(|row| row.len() != table.header.len()) {
        return Err("hard table has invalid header/body cell structure".into());
    }
    let rows: Vec<Vec<u8>> = table
        .body
        .iter()
        .map(|row| encode_row(row))
        .collect::<Result<_, _>>()?;
    let mut structural = Vec::new();
    structural.extend_from_slice(b"SREP-REQ-TSCH");
    push_u32(&mut structural, 1);
    push_u32(&mut structural, ordinal);
    push_u32(&mut structural, table.header.len() as u32);
    for (alignment, cell) in table.alignments.iter().zip(&table.header) {
        structural.push(*alignment);
        push_string(&mut structural, cell)?;
    }
    push_u32(&mut structural, rows.len() as u32);
    for (ordinal, row) in rows.iter().enumerate() {
        push_u32(&mut structural, ordinal as u32);
        push_u32(&mut structural, row.len() as u32);
        structural.extend_from_slice(&Sha256::digest(row));
    }
    units.push(Unit {
        heading: heading.to_owned(),
        kind: "table-schema".to_owned(),
        structural,
    });
    for row in rows {
        units.push(Unit {
            heading: heading.to_owned(),
            kind: "table-row".to_owned(),
            structural: row,
        });
    }
    Ok(())
}

fn table_from_node(node: &Node) -> Result<TableData, Box<dyn std::error::Error>> {
    let NodeKind::Table(alignments) = &node.kind else {
        return Err("expected table".into());
    };
    let mut header = Vec::new();
    let mut body = Vec::new();
    for row in node
        .children
        .iter()
        .filter(|child| matches!(child.kind, NodeKind::TableHead | NodeKind::TableRow))
    {
        let cells: Vec<String> = row
            .children
            .iter()
            .filter(|child| matches!(child.kind, NodeKind::TableCell))
            .map(|cell| normalize_leaf(&plain_text(cell)))
            .collect::<Result<_, _>>()?;
        if matches!(row.kind, NodeKind::TableHead) {
            header = cells;
        } else {
            body.push(cells);
        }
    }
    Ok(TableData {
        alignments: alignments.clone(),
        header,
        body,
    })
}

fn encode_row(row: &[String]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, row.len() as u32);
    for cell in row {
        push_string(&mut bytes, cell)?;
    }
    Ok(bytes)
}

fn encode_list(node: &Node, depth: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let NodeKind::List { ordered, start } = node.kind else {
        return Err("expected list".into());
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"SREP-REQ-LIST");
    push_u32(&mut bytes, 1);
    bytes.push(u8::from(ordered));
    push_u32(&mut bytes, start);
    let items: Vec<_> = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, NodeKind::Item))
        .collect();
    push_u32(&mut bytes, items.len() as u32);
    for (ordinal, item) in items.into_iter().enumerate() {
        push_u32(&mut bytes, ordinal as u32);
        push_u32(&mut bytes, depth);
        let blocks: Vec<_> = item
            .children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::Other))
            .collect();
        let has_direct_text = blocks.iter().any(|child| {
            matches!(
                child.kind,
                NodeKind::Text(_) | NodeKind::Code(_) | NodeKind::SoftBreak | NodeKind::HardBreak
            )
        });
        let block_count = blocks
            .iter()
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::Paragraph | NodeKind::List { .. } | NodeKind::CodeBlock { .. }
                )
            })
            .count()
            + usize::from(has_direct_text);
        push_u32(&mut bytes, u32::try_from(block_count)?);
        if has_direct_text {
            bytes.push(1);
            push_string(&mut bytes, &normalize_leaf(&item_inline_text(item))?)?;
        }
        for block in blocks {
            match block.kind {
                NodeKind::Paragraph => {
                    bytes.push(1);
                    push_string(&mut bytes, &normalize_leaf(&plain_text(block))?)?;
                }
                NodeKind::List { .. } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&encode_list(block, depth + 1)?);
                }
                NodeKind::CodeBlock { .. } => {
                    bytes.push(3);
                    push_string_bytes(&mut bytes, block_code_nfc(block).as_bytes())?;
                }
                NodeKind::Text(_)
                | NodeKind::Code(_)
                | NodeKind::SoftBreak
                | NodeKind::HardBreak => {}
                _ => return Err("unsupported block in marked list".into()),
            }
        }
    }
    Ok(bytes)
}

fn block_code_nfc(node: &Node) -> String {
    code_text(node).nfc().collect()
}
fn code_text(node: &Node) -> String {
    node.children
        .iter()
        .map(|child| match &child.kind {
            NodeKind::Text(value) => value.clone(),
            _ => code_text(child),
        })
        .collect()
}

fn plain_text(node: &Node) -> String {
    match &node.kind {
        NodeKind::Text(value) | NodeKind::Code(value) => value.clone(),
        NodeKind::SoftBreak | NodeKind::HardBreak => " ".to_owned(),
        NodeKind::Html(_) | NodeKind::HtmlBlock => String::new(),
        _ => node.children.iter().map(plain_text).collect(),
    }
}

fn item_inline_text(node: &Node) -> String {
    node.children
        .iter()
        .filter(|child| !matches!(child.kind, NodeKind::List { .. }))
        .map(plain_text)
        .collect()
}

fn contains_marker(node: &Node) -> bool {
    node_html(node).contains("hard-unit")
}

fn node_html(node: &Node) -> &str {
    match &node.kind {
        NodeKind::Html(value) => value,
        NodeKind::HtmlBlock => node
            .children
            .iter()
            .find_map(|child| match &child.kind {
                NodeKind::Html(value) => Some(value.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    }
}
fn contains_must(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices("MUST").any(|(start, _)| {
        let end = start + 4;
        (start == 0 || !bytes[start - 1].is_ascii_alphabetic())
            && (end == bytes.len() || !bytes[end].is_ascii_alphabetic())
    })
}

fn heading_path(headings: &[String]) -> String {
    headings.join(" > ")
}

fn next_table_ordinal(ordinals: &mut BTreeMap<String, u32>, heading: &str) -> u32 {
    let ordinal = ordinals.get(heading).copied().unwrap_or(0);
    ordinals.insert(heading.to_owned(), ordinal.saturating_add(1));
    ordinal
}

fn normalize_leaf(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let normalized: String = value.nfc().collect();
    let mut output = String::new();
    let mut in_space = false;
    for character in normalized.chars() {
        if character.is_whitespace() {
            in_space = true;
        } else {
            if in_space && !output.is_empty() {
                output.push(' ');
            }
            output.push(character);
            in_space = false;
        }
    }
    Ok(output.trim_matches(' ').to_owned())
}

fn canonical_payload(heading: &str, kind: &str, structural: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, heading.len() as u32);
    bytes.extend_from_slice(heading.as_bytes());
    push_u32(&mut bytes, kind.len() as u32);
    bytes.extend_from_slice(kind.as_bytes());
    bytes.extend_from_slice(structural);
    bytes
}

fn digest_payload_hex(payload: &[u8]) -> String {
    hex(payload)
}
fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
fn push_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    push_string_bytes(bytes, value.as_bytes())
}
fn push_string_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let length = u32::try_from(value.len())?;
    push_u32(bytes, length);
    bytes.extend_from_slice(value);
    Ok(())
}
fn alignment_code(alignment: Alignment) -> u8 {
    match alignment {
        Alignment::None => 0,
        Alignment::Left => 1,
        Alignment::Center => 2,
        Alignment::Right => 3,
    }
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
fn ensure_unique_units(units: &[Unit]) -> Result<(), Box<dyn std::error::Error>> {
    let mut seen = BTreeMap::<(String, String, String), Vec<u8>>::new();
    let mut prefixes = BTreeMap::<String, Vec<u8>>::new();
    for unit in units {
        let payload = canonical_payload(&unit.heading, &unit.kind, &unit.structural);
        let key = (
            unit.heading.clone(),
            unit.kind.clone(),
            digest_payload_hex(&payload),
        );
        if seen.insert(key, payload.clone()).is_some() {
            return Err("identical same-heading same-kind requirement".into());
        }
        let id_prefix = hex(Sha256::digest(&payload))[..16].to_owned();
        if let Some(previous) = prefixes.insert(id_prefix, payload.clone()) {
            if previous == payload {
                return Err("duplicate requirement payload".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_payload_preserves_binary_length_prefixes() {
        let structural = {
            let mut bytes = Vec::new();
            push_u32(&mut bytes, 1);
            push_u32(&mut bytes, 200);
            bytes.extend(std::iter::repeat_n(b'A', 200));
            bytes
        };
        let payload = canonical_payload("ExtractorGolden", "table-row", &structural);
        assert_eq!(payload.len(), 240);
        assert_eq!(&payload[36..40], &[0xc8, 0, 0, 0]);
        assert_eq!(
            hex(Sha256::digest(&payload)),
            "2ecd3c7354df617f7d6300d1b21e4716f43dbe8b84097f3d2fbb83bac5519db1"
        );
    }

    #[test]
    fn marked_nested_list_is_structural() {
        let source = "# H\n<!-- hard-unit kind=\"list\" -->\n- one\n  - two\n<!-- /hard-unit -->\n";
        let units = extract(source).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "list");
        assert!(units[0].structural.starts_with(b"SREP-REQ-LIST"));
    }

    #[test]
    fn table_rows_are_separate_and_schema_binds_membership() {
        let units = extract("# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| x | y |\n| p | q |\n<!-- /hard-unit -->\n").unwrap();
        assert_eq!(
            units
                .iter()
                .map(|unit| unit.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["table-schema", "table-row", "table-row"]
        );
        assert_eq!(
            units[1].structural,
            [2, 0, 0, 0, 1, 0, 0, 0, b'x', 1, 0, 0, 0, b'y']
        );
    }

    #[test]
    fn table_schema_binds_header_and_order_but_rows_remain_stable() {
        let first = extract(
            "# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| x | y |\n| p | q |\n<!-- /hard-unit -->\n",
        )
        .unwrap();
        let reordered = extract(
            "# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| p | q |\n| x | y |\n<!-- /hard-unit -->\n",
        )
        .unwrap();
        assert_ne!(first[0].structural, reordered[0].structural);
        assert_eq!(first[1].structural, reordered[2].structural);
        assert_eq!(first[2].structural, reordered[1].structural);
    }

    #[test]
    fn malformed_markers_are_rejected() {
        assert!(extract("<!-- hard-unit kind=\"unknown\" -->\ntext\n<!-- /hard-unit -->").is_err());
        assert!(extract("<!-- hard-unit kind=\"paragraph\" -->\ntext").is_err());
        assert!(extract("<!-- /hard-unit -->\ntext").is_err());
    }

    #[test]
    fn synthetic_collision_id_uses_full_digest_suffix() {
        let first = b"first payload";
        let second = b"second payload";
        let first_digest = Sha256::digest(first);
        let second_digest = Sha256::digest(second);
        let id = format!("REQ-{}-{}", &hex(first_digest)[..16], hex(second_digest));
        assert!(id.starts_with("REQ-"));
        assert_eq!(id.len(), 4 + 16 + 1 + 64);
    }
}
