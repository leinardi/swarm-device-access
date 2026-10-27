/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! The strict config-file reader: yaml.v3's node tree, built from saphyr-parser's events the way
//! yaml.v3's `parser` builds it, then the two passes of `LoadFile` over it: the shape pass
//! (`checkShapes`) and the strict decode into the file schema (yaml.v3's `decoder` with
//! `KnownFields`, for the field types the schema has).
//!
//! saphyr-parser reads YAML 1.2 and yaml.v3 YAML 1.1, so two documents saphyr accepts are refused
//! here as yaml.v3 refuses them: a `%YAML` directive for any version but 1.1, and a `...` before
//! the first document. A syntax error keeps yaml.v3's `yaml: ` prefix; its text is saphyr's (Q6).

use std::collections::HashMap;
use std::fmt;

use saphyr_parser::{Event, Parser, ScalarStyle, ScanError, Span, StrInput};

use super::FileSchema;
use super::resolve::{
    BINARY_TAG, MAP_TAG, MERGE_TAG, NULL_TAG, Resolved, SEQ_TAG, STR_TAG, decode_base64, resolve,
    short_tag,
};
use crate::go_compat::quote::quote;

/// A YAML syntax error, as yaml.v3 reports a scanner or parser failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    message: String,
    /// The anchor of an alias to an anchor this document does not define.
    unknown_anchor: Option<String>,
}

impl SyntaxError {
    fn scan(error: &ScanError) -> Self {
        Self::scan_line(error.marker().line(), error.info())
    }

    /// A scanner error, with its 1-based line.
    fn scan_line(line: usize, message: &str) -> Self {
        Self::plain(&format!("line {line}: {message}"))
    }

    /// yaml.v3's error for an alias to an undefined anchor.
    fn unknown_anchor(name: &str) -> Self {
        Self {
            message: format!("unknown anchor '{name}' referenced"),
            unknown_anchor: Some(name.to_owned()),
        }
    }

    /// A file that is not UTF-8.
    pub(super) fn invalid_utf8() -> Self {
        Self::plain("invalid UTF-8")
    }

    /// A libyaml parser error, as yaml.v3 words it: its line is the 0-based line of the
    /// problem, and is left out on the first line.
    fn parser(line: usize, message: &str) -> Self {
        if line == 0 {
            Self::plain(message)
        } else {
            Self::plain(&format!("line {line}: {message}"))
        }
    }

    fn plain(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            unknown_anchor: None,
        }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "yaml: {}", self.message)
    }
}

impl std::error::Error for SyntaxError {}

/// What the shape pass refuses (`checkShapes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    /// The document is not a mapping.
    NotAMapping,
    /// A merge key.
    MergeKey,
    /// An alias that leads back to itself, under `key` when it is a value's.
    AliasCycle {
        /// The key whose value it is.
        key: Option<String>,
    },
    /// A known key whose value does not have its shape.
    WrongShape {
        /// The key.
        key: String,
        /// What the value must be.
        shape: Shape,
    },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAMapping => formatter.write_str("the document must be a mapping"),
            Self::MergeKey => formatter.write_str("YAML merge keys are not supported"),
            Self::AliasCycle { key: None } => formatter.write_str("alias cycle"),
            Self::AliasCycle { key: Some(key) } => {
                write!(formatter, "key {}: alias cycle", quote(key))
            }
            Self::WrongShape { key, shape } => {
                write!(formatter, "key {} must be {shape}: wrong shape", quote(key))
            }
        }
    }
}

impl std::error::Error for ShapeError {}

/// What a known key's value must look like (`keyShape`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `true` or `false`.
    Bool,
    /// A string that is not empty.
    NonEmptyString,
    /// Any string.
    String,
    /// A list of strings that are not empty.
    NonEmptyStringList,
}

impl fmt::Display for Shape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bool => "a boolean (true or false)",
            Self::NonEmptyString => "a non-empty string",
            Self::String => "a string (empty disables it)",
            Self::NonEmptyStringList => "a list of non-empty strings",
        })
    }
}

/// A failed strict decode, as yaml.v3's `Decode` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The type errors (`yaml.TypeError`), each `line N: ...`.
    Unmarshal(Vec<String>),
    /// A failure that stopped the decode (yaml.v3's `failf`).
    Fail(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmarshal(errors) => {
                write!(
                    formatter,
                    "yaml: unmarshal errors:\n  {}",
                    errors.join("\n  ")
                )
            }
            Self::Fail(message) => write!(formatter, "yaml: {message}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The schema's keys, their shapes and their field indices.
pub(super) const KEYS: [(&str, Shape); 10] = [
    ("log-format", Shape::NonEmptyString),
    ("log-level", Shape::NonEmptyString),
    ("log-time", Shape::Bool),
    ("docker-socket", Shape::NonEmptyString),
    ("dry-run", Shape::Bool),
    ("policy-mode", Shape::NonEmptyString),
    ("device-allow", Shape::NonEmptyStringList),
    ("device-deny", Shape::NonEmptyStringList),
    ("metrics-addr", Shape::String),
    ("debug-addr", Shape::String),
];

/// The Go type name the decode errors give the schema.
const SCHEMA_TYPE: &str = "config.FileSchema";

/// A node's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Document,
    Sequence,
    Mapping,
    Scalar,
    Alias,
}

/// A yaml.v3 `Node`, in an arena.
#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    /// The short tag; empty for an alias.
    tag: String,
    /// A scalar's text, or an alias's anchor name.
    value: String,
    /// Whether a scalar was quoted or a block scalar.
    quoted: bool,
    /// The 1-based line.
    line: usize,
    content: Vec<usize>,
    /// An alias's target.
    alias: Option<usize>,
}

/// One document's node tree, built from the events.
#[derive(Debug, Default)]
pub(super) struct Tree {
    nodes: Vec<Node>,
    /// The document node, when there was a document.
    document: Option<usize>,
}

/// The events of one file, read a document at a time.
pub(super) struct Reader<'input> {
    parser: Parser<'input, StrInput<'input>>,
    input: &'input str,
    /// The byte offset of each character, for the positions saphyr reports in characters.
    offsets: Vec<usize>,
    /// The node each anchor id names in the current document.
    anchors: HashMap<usize, usize>,
    /// Where the text between documents starts: the stream start, then each document end.
    boundary: usize,
    /// How many documents were read.
    documents: usize,
    /// Where directives follow a document that ended without `...`, which saphyr refuses as
    /// the document's error and libyaml reads as the start of the next document.
    directives_after_implicit_end: Option<usize>,
}

impl<'input> Reader<'input> {
    /// A reader over `input`.
    ///
    /// # Errors
    ///
    /// libyaml's refusal of a `...` before the first document, which saphyr does not make.
    pub(super) fn new(input: &'input str) -> Result<Self, SyntaxError> {
        check_leading_document_end(input)?;
        Ok(Self {
            parser: Parser::new_from_str(input),
            input,
            offsets: input
                .char_indices()
                .map(|(offset, _)| offset)
                .chain([input.len()])
                .collect(),
            anchors: HashMap::new(),
            boundary: 0,
            documents: 0,
            directives_after_implicit_end: None,
        })
    }

    /// The byte offset of a character index.
    fn offset(&self, index: usize) -> usize {
        self.offsets.get(index).copied().unwrap_or(self.input.len())
    }

    fn next(&mut self) -> Result<(Event<'input>, Span), SyntaxError> {
        match self.parser.next_event() {
            Some(Ok(event)) => Ok(event),
            Some(Err(error)) => Err(self.scan_error(&error)),
            None => Ok((Event::StreamEnd, Span::default())),
        }
    }

    /// yaml.v3's `parser.parse` for the next document: its tree, empty at the end of the stream.
    ///
    /// # Errors
    ///
    /// A syntax error in the document, or in the directives before it.
    pub(super) fn document(&mut self) -> Result<Tree, SyntaxError> {
        let mut tree = Tree::default();
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::StreamStart | Event::Nothing => {}
                Event::StreamEnd => return Ok(tree),
                Event::DocumentStart(_) => {
                    self.check_directives(self.boundary, self.offset(span.start.index()))?;
                    self.anchors.clear();
                    let document = tree.push(Kind::Document, String::new(), span.start.line());
                    let root = self.build(&mut tree)?;
                    tree.nodes[document].content.push(root);
                    // The document end; nothing else can follow a document's root.
                    match self.parser.next_event() {
                        Some(Err(error)) if error.info() == DIRECTIVE_AFTER_IMPLICIT_END => {
                            let at = self.offset(error.marker().index());
                            self.directives_after_implicit_end = Some(at);
                            self.boundary = at;
                        }
                        Some(Err(error)) => return Err(self.scan_error(&error)),
                        Some(Ok((_, end))) => self.boundary = self.offset(end.start.index()),
                        None => {}
                    }
                    tree.document = Some(document);
                    self.documents += 1;
                    return Ok(tree);
                }
                Event::DocumentEnd
                | Event::Alias(_)
                | Event::Scalar(..)
                | Event::SequenceStart(..)
                | Event::SequenceEnd
                | Event::MappingStart(..)
                | Event::MappingEnd => {
                    return Err(SyntaxError::plain("did not find expected <document start>"));
                }
            }
        }
    }

    /// A document's root node and everything under it, without recursion, refusing nesting
    /// deeper than libyaml's limit.
    fn build(&mut self, tree: &mut Tree) -> Result<usize, SyntaxError> {
        let mut open: Vec<usize> = Vec::new();
        loop {
            let (event, span) = self.next()?;
            let line = span.start.line();
            let (index, opens) = match event {
                Event::SequenceEnd | Event::MappingEnd => {
                    let Some(closed) = open.pop() else {
                        return Err(SyntaxError::plain("did not find expected node content"));
                    };
                    if open.is_empty() {
                        return Ok(closed);
                    }
                    continue;
                }
                Event::Scalar(value, style, anchor, tag) => {
                    let line = self.scalar_line(style, span);
                    let quoted = !matches!(style, ScalarStyle::Plain);
                    let tag = match explicit_tag(tag.as_deref()) {
                        Some(tag) => tag,
                        None if quoted => STR_TAG.to_owned(),
                        None if value == "<<" => MERGE_TAG.to_owned(),
                        None => plain_tag(&value),
                    };
                    let index = tree.push(Kind::Scalar, tag, line);
                    tree.nodes[index].value = value.into_owned();
                    tree.nodes[index].quoted = quoted;
                    self.anchor(anchor, index);
                    (index, false)
                }
                Event::Alias(anchor) => {
                    let index = tree.push(Kind::Alias, String::new(), line);
                    tree.nodes[index].value = self.alias_name(span);
                    let Some(&target) = self.anchors.get(&anchor) else {
                        return Err(SyntaxError::unknown_anchor(&tree.nodes[index].value));
                    };
                    tree.nodes[index].alias = Some(target);
                    (index, false)
                }
                Event::SequenceStart(anchor, tag) => {
                    let tag = explicit_tag(tag.as_deref()).unwrap_or_else(|| SEQ_TAG.to_owned());
                    let index = tree.push(Kind::Sequence, tag, line);
                    self.anchor(anchor, index);
                    (index, true)
                }
                Event::MappingStart(anchor, tag) => {
                    let tag = explicit_tag(tag.as_deref()).unwrap_or_else(|| MAP_TAG.to_owned());
                    let index = tree.push(Kind::Mapping, tag, line);
                    self.anchor(anchor, index);
                    (index, true)
                }
                Event::Nothing
                | Event::StreamStart
                | Event::StreamEnd
                | Event::DocumentStart(_)
                | Event::DocumentEnd => {
                    return Err(SyntaxError::plain("did not find expected node content"));
                }
            };
            match open.last() {
                Some(&parent) => tree.nodes[parent].content.push(index),
                None if !opens => return Ok(index),
                None => {}
            }
            if opens {
                open.push(index);
                if open.len() > MAX_DEPTH {
                    return Err(SyntaxError::scan_line(line, "exceeded max depth of 10000"));
                }
            }
        }
    }

    /// A saphyr error as yaml.v3 reports it: an alias to an anchor never defined is yaml.v3's own
    /// `unknown anchor '<name>' referenced`, without a line; anything else is Q6.
    fn scan_error(&self, error: &ScanError) -> SyntaxError {
        if error.info() == "while parsing node, found unknown anchor" {
            let start = self.offset(error.marker().index() + 1);
            let name: String = self.input[start..]
                .chars()
                .take_while(|&character| !is_anchor_end(character))
                .collect();
            return SyntaxError::unknown_anchor(&name);
        }
        SyntaxError::scan(error)
    }

    /// libyaml's refusal of a `%YAML` directive for any version but 1.1 (`found incompatible
    /// YAML document`), which saphyr does not make, for the directives between `start` and
    /// `end`: the text between two documents, where only comments, directives and document
    /// markers can be.
    fn check_directives(&self, start: usize, end: usize) -> Result<(), SyntaxError> {
        let mut position = start;
        let mut line_number = self.input[..start].matches('\n').count();
        for line in self.input[start..end].split_inclusive('\n') {
            let at_line_start = position == 0 || self.input.as_bytes()[position - 1] == b'\n';
            let number = line_number;
            position += line.len();
            line_number += usize::from(line.ends_with('\n'));
            if !at_line_start {
                continue;
            }
            let line = line.trim_end_matches(['\n', '\r']);
            let line = line.strip_prefix('\u{feff}').unwrap_or(line);
            let Some(version) = line.strip_prefix("%YAML") else {
                continue;
            };
            if !version.starts_with([' ', '\t']) {
                continue;
            }
            let version: String = version
                .trim_start_matches([' ', '\t'])
                .chars()
                .take_while(|character| !matches!(character, ' ' | '\t' | '#'))
                .collect();
            if version != "1.1" {
                return Err(SyntaxError::parser(
                    number,
                    "found incompatible YAML document",
                ));
            }
        }
        Ok(())
    }

    /// The line yaml.v3 gives a scalar: a block scalar's is the line of its `|` or `>`
    /// indicator, where saphyr's span starts at the content.
    fn scalar_line(&self, style: ScalarStyle, span: Span) -> usize {
        let line = span.start.line();
        if !matches!(style, ScalarStyle::Literal | ScalarStyle::Folded) {
            return line;
        }
        let before = &self.input[..self.offset(span.start.index())];
        match before.rfind(['|', '>']) {
            Some(indicator) => line.saturating_sub(before[indicator..].matches('\n').count()),
            None => line,
        }
    }

    /// Registers a node under its anchor id when it has one; a later anchor of the same name
    /// replaces it, as in yaml.v3.
    fn anchor(&mut self, anchor: usize, index: usize) {
        if anchor != 0 {
            self.anchors.insert(anchor, index);
        }
    }

    /// An alias's anchor name: its span's text without the `*`.
    fn alias_name(&self, span: Span) -> String {
        let start = self.offset(span.start.index() + 1);
        let end = self.offset(span.end.index());
        self.input.get(start..end).unwrap_or_default().to_owned()
    }

    /// Whether the stream ends after the first document, as `singleDocument` checks: `Ok(true)`
    /// at the end, `Ok(false)` when another document parses.
    ///
    /// yaml.v3 keeps anchors across documents and saphyr does not, so an alias in a later
    /// document to an anchor of an earlier one, which yaml.v3 resolves, is taken as the document
    /// parsing.
    ///
    /// # Errors
    ///
    /// A syntax error in the next document.
    pub(super) fn at_end(&mut self) -> Result<bool, SyntaxError> {
        if let Some(start) = self.directives_after_implicit_end.take() {
            return self.directives_then_document(start);
        }
        let start = self.boundary;
        match self.document() {
            Ok(tree) => Ok(tree.document.is_none()),
            Err(error) => match &error.unknown_anchor {
                Some(name) if self.anchored_before(name, start) => Ok(false),
                Some(_) | None => Err(error),
            },
        }
    }

    /// libyaml's reading of directives after a document that ended without `...`: they belong
    /// to the next document, which must start with `---`. saphyr stops there, so the directives
    /// are checked here and a following `---` is taken as the next document parsing.
    fn directives_then_document(&self, start: usize) -> Result<bool, SyntaxError> {
        let rest = &self.input[start..];
        let mut directives_end = start;
        for line in rest.split_inclusive('\n') {
            let content = line.trim_start_matches([' ', '\t']);
            if !(line.starts_with('%') || content.trim_end().is_empty() || content.starts_with('#'))
            {
                break;
            }
            directives_end += line.len();
        }
        self.check_directives(start, directives_end)?;
        let next = &self.input[directives_end..];
        if next.starts_with("---") && next[3..].chars().next().is_none_or(char::is_whitespace) {
            Ok(false)
        } else {
            Err(SyntaxError::plain("did not find expected <document start>"))
        }
    }

    /// Whether `&name` appears in the text before `end`.
    fn anchored_before(&self, name: &str, end: usize) -> bool {
        let anchor = format!("&{name}");
        self.input[..end]
            .match_indices(&anchor)
            .any(|(position, _)| {
                self.input[position + anchor.len()..]
                    .chars()
                    .next()
                    .is_none_or(is_anchor_end)
            })
    }
}

/// saphyr's error for a directive after a document that ended without `...`.
const DIRECTIVE_AFTER_IMPLICIT_END: &str = "missing explicit document end marker before directive";

/// libyaml's limit on nested collections (`max_flow_level`, `max_indents`).
const MAX_DEPTH: usize = 10_000;
impl Tree {
    fn push(&mut self, kind: Kind, tag: String, line: usize) -> usize {
        self.nodes.push(Node {
            kind,
            tag,
            value: String::new(),
            quoted: false,
            line,
            content: Vec::new(),
            alias: None,
        });
        self.nodes.len() - 1
    }

    /// The document's root, when there is a document.
    fn root(&self) -> Option<usize> {
        self.document
            .and_then(|document| self.nodes[document].content.first().copied())
    }

    /// `isEmptyDocument`: no document, or one whose root is the empty plain null.
    pub(super) fn is_empty_document(&self) -> bool {
        self.root().is_none_or(|root| {
            let node = &self.nodes[root];
            node.kind == Kind::Scalar && node.tag == NULL_TAG && node.value.is_empty()
        })
    }

    /// `deref`: follows aliases to the node they name. An anchor cannot sit on an alias, so a
    /// chain is one step long; the visited list is Go's cycle guard, kept for its error.
    fn deref(&self, mut index: usize) -> Result<usize, ()> {
        let mut visited = Vec::new();
        while self.nodes[index].kind == Kind::Alias {
            if visited.contains(&index) {
                return Err(());
            }
            visited.push(index);
            index = self.nodes[index].alias.ok_or(())?;
        }
        Ok(index)
    }

    /// `checkShapes`: the root is a mapping without merge keys, and every known key's value has
    /// its shape. Unknown keys are left to the strict decode.
    ///
    /// # Errors
    ///
    /// The first violation.
    pub(super) fn check_shapes(&self) -> Result<(), ShapeError> {
        let Some(root) = self.root() else {
            return Err(ShapeError::NotAMapping);
        };
        let root = self
            .deref(root)
            .map_err(|()| ShapeError::AliasCycle { key: None })?;
        if self.nodes[root].kind != Kind::Mapping {
            return Err(ShapeError::NotAMapping);
        }
        for pair in self.nodes[root].content.chunks(2) {
            let [key, value] = *pair else { continue };
            let key = &self.nodes[key];
            if key.tag == MERGE_TAG || key.value == "<<" {
                return Err(ShapeError::MergeKey);
            }
            let Some(&(name, shape)) = KEYS.iter().find(|(name, _)| *name == key.value) else {
                continue;
            };
            let value = self.deref(value).map_err(|()| ShapeError::AliasCycle {
                key: Some(name.to_owned()),
            })?;
            if !self.has_shape(value, shape) {
                return Err(ShapeError::WrongShape {
                    key: name.to_owned(),
                    shape,
                });
            }
        }
        Ok(())
    }

    /// `hasShape`, for a node whose aliases were followed.
    fn has_shape(&self, index: usize, shape: Shape) -> bool {
        let node = &self.nodes[index];
        match shape {
            Shape::Bool => node.kind == Kind::Scalar && node.tag == super::resolve::BOOL_TAG,
            Shape::NonEmptyString => {
                node.kind == Kind::Scalar && node.tag == STR_TAG && !node.value.is_empty()
            }
            Shape::String => node.kind == Kind::Scalar && node.tag == STR_TAG,
            Shape::NonEmptyStringList => {
                node.kind == Kind::Sequence
                    && node.content.iter().all(|&item| {
                        self.deref(item)
                            .is_ok_and(|item| self.has_shape(item, Shape::NonEmptyString))
                    })
            }
        }
    }

    /// The strict decode (`Decoder.Decode` with `KnownFields`) of the document into the schema.
    ///
    /// # Errors
    ///
    /// The type errors, or the failure that stopped the decode.
    pub(super) fn decode(&self) -> Result<FileSchema, DecodeError> {
        let mut schema = FileSchema::default();
        let mut decoder = Decoder {
            tree: self,
            errors: Vec::new(),
            active_aliases: vec![false; self.nodes.len()],
            decode_count: 0,
            alias_count: 0,
            alias_depth: 0,
        };
        if let Some(document) = self.document {
            decoder.count()?;
            decoder.unmarshal(document, &mut Target::Schema(&mut schema))?;
        }
        if decoder.errors.is_empty() {
            Ok(schema)
        } else {
            Err(DecodeError::Unmarshal(decoder.errors))
        }
    }
}

/// Where an anchor or alias name ends: at white space or a flow indicator, as the scanner reads
/// it.
fn is_anchor_end(character: char) -> bool {
    character.is_whitespace() || matches!(character, ',' | '[' | ']' | '{' | '}')
}

/// `p.node`'s tag for an explicit tag: none for an absent or the non-specific `!`, else its short
/// form.
fn explicit_tag(tag: Option<&saphyr_parser::Tag>) -> Option<String> {
    let tag = tag?;
    let full = format!("{}{}", tag.handle, tag.suffix);
    (!full.is_empty() && full != "!").then(|| short_tag(&full))
}

/// The tag a plain scalar without an explicit tag resolves to.
fn plain_tag(value: &str) -> String {
    // resolve fails only under an explicit tag.
    resolve("", value).map_or_else(|_| STR_TAG.to_owned(), |(tag, _)| tag)
}

/// libyaml's refusal of a `...` before the first document, which saphyr does not make: libyaml
/// reads it as the start of an implicit document with no node (`did not find expected node
/// content`). The first line that is not blank, a comment or a directive is the first token.
fn check_leading_document_end(input: &str) -> Result<(), SyntaxError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let first = input
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .enumerate()
        .find(|(_, line)| {
            let content = line.trim_start_matches([' ', '\t']);
            !content.is_empty() && !content.starts_with('#') && !line.starts_with('%')
        });
    if let Some((index, line)) = first
        && let Some(rest) = line.strip_prefix("...")
        && (rest.is_empty() || rest.starts_with([' ', '\t']))
    {
        return Err(SyntaxError::parser(
            index,
            "did not find expected node content",
        ));
    }
    Ok(())
}

/// Where a decoded value goes: the schema's field types.
enum Target<'out> {
    Schema(&'out mut FileSchema),
    String(&'out mut String),
    Bool(&'out mut bool),
    OptionalBool(&'out mut Option<bool>),
    List(&'out mut Option<Vec<String>>),
}

impl Target<'_> {
    /// The Go type name the type errors give.
    fn type_name(&self) -> &'static str {
        match self {
            Self::Schema(_) => SCHEMA_TYPE,
            Self::String(_) => "string",
            Self::Bool(_) => "bool",
            Self::OptionalBool(_) => "*bool",
            Self::List(_) => "[]string",
        }
    }
}

/// yaml.v3's `decoder`.
struct Decoder<'tree> {
    tree: &'tree Tree,
    errors: Vec<String>,
    active_aliases: Vec<bool>,
    decode_count: u64,
    alias_count: u64,
    alias_depth: u32,
}

impl Decoder<'_> {
    /// The aliasing guard at the top of `unmarshal`.
    fn count(&mut self) -> Result<(), DecodeError> {
        self.decode_count += 1;
        if self.alias_depth > 0 {
            self.alias_count += 1;
        }
        if self.alias_count > 100
            && self.decode_count > 1000
            && excessive_aliasing(self.alias_count, self.decode_count)
        {
            return Err(DecodeError::Fail(
                "document contains excessive aliasing".to_owned(),
            ));
        }
        Ok(())
    }

    /// `unmarshal`, after the count: whether the value was set.
    fn unmarshal(&mut self, index: usize, target: &mut Target<'_>) -> Result<bool, DecodeError> {
        let node = &self.tree.nodes[index];
        match node.kind {
            Kind::Document => {
                let Some(&root) = node.content.first() else {
                    return Ok(false);
                };
                self.count()?;
                self.unmarshal(root, target)?;
                return Ok(true);
            }
            Kind::Alias => return self.alias(index, target),
            Kind::Scalar | Kind::Sequence | Kind::Mapping => {}
        }
        // prepare: a pointer is allocated and followed unless the node is null.
        if let Target::OptionalBool(slot) = target
            && self.short_tag(index) != NULL_TAG
        {
            let value = slot.insert(false);
            return self.unmarshal_value(index, &mut Target::Bool(value));
        }
        self.unmarshal_value(index, target)
    }

    fn unmarshal_value(
        &mut self,
        index: usize,
        target: &mut Target<'_>,
    ) -> Result<bool, DecodeError> {
        let node = &self.tree.nodes[index];
        match node.kind {
            Kind::Scalar => self.scalar(index, target),
            Kind::Sequence => self.sequence(index, target),
            Kind::Mapping => self.mapping(index, target),
            Kind::Document | Kind::Alias => Ok(false),
        }
    }

    /// `alias`.
    fn alias(&mut self, index: usize, target: &mut Target<'_>) -> Result<bool, DecodeError> {
        let node = &self.tree.nodes[index];
        if self.active_aliases[index] {
            return Err(DecodeError::Fail(format!(
                "anchor '{}' value contains itself",
                node.value
            )));
        }
        let Some(aliased) = node.alias else {
            return Ok(false);
        };
        self.active_aliases[index] = true;
        self.alias_depth += 1;
        let result = self.count().and_then(|()| self.unmarshal(aliased, target));
        self.alias_depth -= 1;
        self.active_aliases[index] = false;
        result
    }

    /// `Node.ShortTag`.
    fn short_tag(&self, index: usize) -> String {
        let node = &self.tree.nodes[index];
        if indicated_string(node) {
            return STR_TAG.to_owned();
        }
        match node.kind {
            Kind::Alias => node
                .alias
                .map_or_else(String::new, |aliased| self.short_tag(aliased)),
            Kind::Document | Kind::Sequence | Kind::Mapping | Kind::Scalar => node.tag.clone(),
        }
    }

    /// `terror`.
    fn type_error(&mut self, index: usize, tag: &str, target: &Target<'_>) {
        let node = &self.tree.nodes[index];
        let tag = if node.tag.is_empty() { tag } else { &node.tag };
        let value = if tag == SEQ_TAG || tag == MAP_TAG {
            String::new()
        } else if node.value.len() > 10 {
            // Go cuts the bytes, possibly inside a character.
            let head = String::from_utf8_lossy(&node.value.as_bytes()[..7]);
            format!(" `{head}...`")
        } else {
            format!(" `{}`", node.value)
        };
        self.errors.push(format!(
            "line {}: cannot unmarshal {}{value} into {}",
            node.line,
            short_tag(tag),
            target.type_name()
        ));
    }

    /// `scalar`.
    fn scalar(&mut self, index: usize, target: &mut Target<'_>) -> Result<bool, DecodeError> {
        let node = &self.tree.nodes[index];
        let (tag, resolved) = if indicated_string(node) {
            (STR_TAG.to_owned(), Resolved::Text(node.value.clone()))
        } else {
            let (tag, resolved) = resolve(&node.tag, &node.value).map_err(DecodeError::Fail)?;
            if tag == BINARY_TAG {
                let Resolved::Text(encoded) = resolved else {
                    return Ok(false);
                };
                let decoded = decode_base64(&encoded).ok_or_else(|| {
                    DecodeError::Fail("!!binary value contains invalid base64 data".to_owned())
                })?;
                (
                    tag,
                    Resolved::Text(String::from_utf8_lossy(&decoded).into_owned()),
                )
            } else {
                (tag, resolved)
            }
        };
        if resolved == Resolved::Null {
            return Ok(null(target));
        }
        match (target, &resolved) {
            (Target::String(out), Resolved::Text(text)) if tag == BINARY_TAG => {
                (*out).clone_from(text);
                Ok(true)
            }
            (Target::String(out), _) => {
                (**out).clone_from(&node.value);
                Ok(true)
            }
            (Target::Bool(out), Resolved::Bool(flag)) => {
                **out = *flag;
                Ok(true)
            }
            (Target::Bool(out), Resolved::Text(text)) if yaml_1_1_bool(text).is_some() => {
                **out = yaml_1_1_bool(text).unwrap_or_default();
                Ok(true)
            }
            (target, _) => {
                self.type_error(index, &tag, target);
                Ok(false)
            }
        }
    }

    /// `sequence`.
    fn sequence(&mut self, index: usize, target: &mut Target<'_>) -> Result<bool, DecodeError> {
        let Target::List(out) = target else {
            self.type_error(index, SEQ_TAG, target);
            return Ok(false);
        };
        let mut items = Vec::new();
        for &item in &self.tree.nodes[index].content {
            let mut text = String::new();
            self.count()?;
            if self.unmarshal(item, &mut Target::String(&mut text))? {
                items.push(text);
            }
        }
        **out = Some(items);
        Ok(true)
    }

    /// `mapping`: duplicate keys first, then the schema's fields.
    fn mapping(&mut self, index: usize, target: &mut Target<'_>) -> Result<bool, DecodeError> {
        let node = &self.tree.nodes[index];
        let before = self.errors.len();
        let keys: Vec<usize> = node.content.iter().step_by(2).copied().collect();
        for (position, &first) in keys.iter().enumerate() {
            for &second in &keys[position + 1..] {
                let (first, second) = (&self.tree.nodes[first], &self.tree.nodes[second]);
                if first.kind == second.kind && first.value == second.value {
                    self.errors.push(format!(
                        "line {}: mapping key {} already defined at line {}",
                        second.line,
                        quote(&second.value),
                        first.line
                    ));
                }
            }
        }
        if self.errors.len() > before {
            return Ok(false);
        }
        match target {
            Target::Schema(schema) => self.mapping_struct(index, schema),
            Target::String(_) | Target::Bool(_) | Target::OptionalBool(_) | Target::List(_) => {
                self.type_error(index, MAP_TAG, target);
                Ok(false)
            }
        }
    }

    /// `mappingStruct` with `KnownFields` and unique keys.
    fn mapping_struct(
        &mut self,
        index: usize,
        schema: &mut FileSchema,
    ) -> Result<bool, DecodeError> {
        let mut done = [false; KEYS.len()];
        for pair in self.tree.nodes[index].content.chunks(2) {
            let [key, value] = *pair else { continue };
            // Merge keys never get here: the shape pass refused them.
            let mut name = String::new();
            self.count()?;
            if !self.unmarshal(key, &mut Target::String(&mut name))? {
                continue;
            }
            let line = self.tree.nodes[key].line;
            let Some(field) = KEYS.iter().position(|(known, _)| *known == name) else {
                self.errors.push(format!(
                    "line {line}: field {name} not found in type {SCHEMA_TYPE}"
                ));
                continue;
            };
            if done[field] {
                self.errors.push(format!(
                    "line {line}: field {name} already set in type {SCHEMA_TYPE}"
                ));
                continue;
            }
            done[field] = true;
            let mut target = schema.field(field);
            self.count()?;
            self.unmarshal(value, &mut target)?;
        }
        Ok(true)
    }
}

impl FileSchema {
    /// The field of `KEYS[index]`, as a decode target.
    fn field(&mut self, index: usize) -> Target<'_> {
        match index {
            0 => Target::String(&mut self.log_format),
            1 => Target::String(&mut self.log_level),
            2 => Target::OptionalBool(&mut self.log_time),
            3 => Target::String(&mut self.docker_socket),
            4 => Target::OptionalBool(&mut self.dry_run),
            5 => Target::String(&mut self.policy_mode),
            6 => Target::List(&mut self.device_allow),
            7 => Target::List(&mut self.device_deny),
            8 => Target::String(&mut self.metrics_addr),
            _ => Target::String(&mut self.debug_addr),
        }
    }
}

/// `Node.indicatedString`.
fn indicated_string(node: &Node) -> bool {
    node.kind == Kind::Scalar && (node.tag == STR_TAG || (node.tag.is_empty() && node.quoted))
}

/// `null`: a nil pointer or slice is set; nothing else takes a null.
fn null(target: &mut Target<'_>) -> bool {
    match target {
        Target::OptionalBool(out) => {
            **out = None;
            true
        }
        Target::List(out) => {
            **out = None;
            true
        }
        Target::Schema(_) | Target::String(_) | Target::Bool(_) => false,
    }
}

/// The YAML 1.1 spellings yaml.v3 still decodes into a typed bool.
fn yaml_1_1_bool(text: &str) -> Option<bool> {
    match text {
        "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
        "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
}

/// `allowedAliasRatio` exceeded.
fn excessive_aliasing(alias_count: u64, decode_count: u64) -> bool {
    const LOW: u64 = 400_000;
    const HIGH: u64 = 4_000_000;
    // Exact in f64: both counts stay far below 2^53 for any file this daemon reads.
    #[expect(
        clippy::cast_precision_loss,
        reason = "Go compares the counts as float64; they are far below 2^53"
    )]
    let (aliases, decodes) = (alias_count as f64, decode_count as f64);
    let allowed = if decode_count <= LOW {
        0.99
    } else if decode_count >= HIGH {
        0.10
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "Go scales the ratio in float64 over a range far below 2^53"
        )]
        let scaled = (decode_count - LOW) as f64 / (HIGH - LOW) as f64;
        0.99 - 0.89 * scaled
    };
    aliases / decodes > allowed
}
