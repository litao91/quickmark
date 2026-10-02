//! Builds a [`FacadeTree`](super::FacadeTree) from comrak's AST.
//!
//! comrak has the right block structure but is missing everything tree-sitter-md exposed as a
//! separate node — heading markers, list markers, `inline` wrappers, the table delimiter row,
//! `section` grouping — and it *removes* two things rules depend on: link reference definitions are
//! detached into a private map, and table cells are autocompleted to the header width. Those are
//! re-derived from raw source in `synth`.

use std::collections::HashMap;

use comrak::nodes::{NodeHeading, NodeList, NodeValue};
// `comrak::Node<'a>` is `&'a AstNode<'a>` — the shared-reference form that `children()` yields.
use comrak::Node as ComrakNode;
use comrak::{parse_document, Arena, Options};

use super::synth::{self, LineIndex};
use super::{FacadeNode, FacadeTree, Kind, LinkTarget};

/// Parses `source` into the tree quickmark's rules walk.
pub fn parse(source: &str) -> FacadeTree {
    let lines = LineIndex::new(source);
    let arena = Arena::new();
    let root = parse_document(&arena, source, &comrak_options());

    let covered = vec![false; lines.line_count()];
    let mut builder = Builder {
        lines,
        nodes: Vec::new(),
        covered,
        enclosing_list: None,
        pending_item_start: None,
        pending_paragraph_start: None,
        math: Vec::new(),
        math_emitted: Vec::new(),
        link_targets: HashMap::new(),
    };
    builder.math = synth::math_regions(&builder.lines);
    clip_math_to_containers(&mut builder.math, &builder.lines, root);
    builder.math_emitted = vec![false; builder.math.len()];
    builder.build(root, source)
}

/// Ends each math region with the container it opened in.
///
/// micromark's mathFlow is a flow construct, so it lives inside whatever container was open where it
/// started and dies with it — `tokenizeNonLazyContinuation` gives up on a line the container no
/// longer covers. An unclosed `$$` inside a list item therefore stops at the item, while the same `$$`
/// at document level runs to the end of the file. [`synth::math_regions`] only sees lines and cannot
/// tell the two apart; comrak's containers can.
fn clip_math_to_containers(math: &mut [synth::Span], lines: &LineIndex<'_>, root: ComrakNode<'_>) {
    for span in math.iter_mut() {
        let opener = span.start().0;
        // Containers holding a row form a chain, so the one that ends soonest is the innermost.
        let innermost = root
            .descendants()
            .filter(|node| {
                matches!(
                    node.data().value,
                    NodeValue::BlockQuote
                        | NodeValue::List(_)
                        | NodeValue::Item(_)
                        | NodeValue::TaskItem(_)
                )
            })
            .map(|node| node.data().sourcepos)
            .filter(|sourcepos| {
                let first = (sourcepos.start.line - 1) as u32;
                let last = (sourcepos.end.line - 1) as u32;
                first <= opener && opener <= last
            })
            .map(|sourcepos| (sourcepos.end.line - 1) as u32)
            .min();
        if let Some(last) = innermost {
            *span = synth::Span::new(span.start(), lines.block_end_row(span.last_row().min(last)));
        }
    }
}

/// The comrak configuration quickmark parses with. Everything not set here stays at its default,
/// which for extensions means off.
///
/// Three of these are load-bearing rather than stylistic:
///
/// - `sourcepos_chars` off, because rules treat columns as UTF-8 byte offsets.
/// - `front_matter_delimiter`, because without it `---\ntitle: x\n---` parses as a thematic break
///   plus a setext heading, and MD041 then reports a missing top-level heading on every file that
///   has front matter.
/// - `tasklist` on, because it moves the item's paragraph start past `[x] `, which is where
///   tree-sitter-md put it. The task marker node itself is deliberately not reproduced — no rule
///   reads it.
pub fn comrak_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.front_matter_delimiter = Some("---".into());
    // markdownlint parses with micromark's `math()` at its defaults, so `$…$` is a math token there
    // and its contents are never emphasis, HTML or a link. Without this, comrak leaves the `$` as
    // text and every inline rule sees through it.
    options.extension.math_dollars = true;
    options.parse.sourcepos_chars = false;
    options.parse.smart = false;
    options.parse.ignore_setext = false;
    options
}

/// Which column a block starts at, decided by what contains it. See [`Builder::start_col`].
#[derive(Debug, Clone, Copy)]
enum Nesting {
    /// A document-level block: column 0, indentation included.
    TopLevel,
    /// A block inside a container: the container's content column.
    Content(u32),
    /// A list item: its own marker, which is what comrak reports.
    OwnMarker,
}

/// A node under construction. Positions stay as row/column and become bytes on flatten, so the
/// synthesis recipes never do that arithmetic themselves.
#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
    /// A list item's content column: where its marker ends *before* that is clamped to the line
    /// length. An empty item's marker node is shorter than the indent its continuation lines need,
    /// and `continuation_prefix` has to use the unclamped column. Zero for every other kind.
    content_col: u32,
    children: Vec<u32>,
}

impl Node {
    fn start(&self) -> (u32, u32) {
        (self.start_row, self.start_col)
    }

    fn end(&self) -> (u32, u32) {
        (self.end_row, self.end_col)
    }

    /// The last row this node's content occupies. An end at column 0 belongs to the row before it.
    fn last_row(&self) -> u32 {
        if self.end_col == 0 {
            self.end_row.saturating_sub(1)
        } else {
            self.end_row
        }
    }
}

/// A comrak `TableCell`'s inline subtree, waiting to be grafted onto a synthesized
/// `pipe_table_cell`. `start_byte` is the graft key; `start`/`end` are the span the `inline` node
/// gets, and `source` is the cell whose children fill it.
struct CellInline<'a> {
    start_byte: u32,
    start: (u32, u32),
    end: (u32, u32),
    source: ComrakNode<'a>,
}

struct Builder<'a> {
    lines: LineIndex<'a>,
    nodes: Vec<Node>,
    /// Lines some emitted block already claims. A link reference definition is only synthesized on a
    /// line no block claimed, so containers deliberately do not claim theirs — their content might be
    /// a definition comrak detached.
    covered: Vec<bool>,
    /// The payload of the list whose items are being emitted, for task list items that carry none.
    enclosing_list: Option<NodeList>,
    /// Start position forced onto the next list item. tree-sitter-md begins a list's *first* item at
    /// the list's own column, which for an indented top-level list is column 0 rather than the
    /// marker's; every later item starts at its marker.
    pending_item_start: Option<(u32, u32)>,
    /// Start position forced onto the next paragraph. Set for a list item whose task marker is not
    /// followed by whitespace — see [`Builder::bare_task_marker`].
    pending_paragraph_start: Option<(u32, u32)>,
    /// The `$$…$$` regions of the document, and whether each one's node has been emitted yet — see
    /// [`Builder::math_at`].
    math: Vec<synth::Span>,
    math_emitted: Vec<bool>,
    /// Destinations of the `link` nodes emitted so far, keyed by build-time index. [`Builder::flatten`]
    /// rekeys them by position.
    link_targets: HashMap<u32, LinkTarget>,
}

impl<'a> Builder<'a> {
    fn build(&mut self, root: ComrakNode<'a>, source: &str) -> FacadeTree {
        let document = self.build_document(root, source);
        self.flatten(document)
    }

    fn add(&mut self, kind: Kind, start: (u32, u32), end: (u32, u32)) -> u32 {
        self.nodes.push(Node {
            kind,
            start_row: start.0,
            start_col: start.1,
            end_row: end.0,
            end_col: end.1,
            content_col: 0,
            children: Vec::new(),
        });
        (self.nodes.len() - 1) as u32
    }

    fn push(&mut self, parent: u32, child: u32) {
        self.nodes[parent as usize].children.push(child);
    }

    fn cover(&mut self, index: u32) {
        let (start_row, last_row) = {
            let node = &self.nodes[index as usize];
            (node.start_row, node.last_row())
        };
        let limit = self.covered.len().saturating_sub(1) as u32;
        for row in start_row..=last_row.min(limit) {
            self.covered[row as usize] = true;
        }
    }

    /// comrak's 1-based inclusive start, converted to tree-sitter's 0-based exclusive one.
    fn start_of(&self, node: ComrakNode<'_>) -> (u32, u32) {
        let sp = node.data().sourcepos;
        ((sp.start.line - 1) as u32, (sp.start.column - 1) as u32)
    }

    /// A block's end, normalized to tree-sitter-md's convention of swallowing the trailing newline:
    /// comrak's end is the block's last character, tree-sitter's is column 0 of the line after it.
    /// At EOF with no trailing newline there is no line after, so the two agree.
    fn block_end(&self, node: ComrakNode<'_>) -> (u32, u32) {
        let sp = node.data().sourcepos;
        if self.lines.has_line(sp.end.line) {
            (sp.end.line as u32, 0)
        } else {
            ((sp.end.line - 1) as u32, sp.end.column as u32)
        }
    }

    /// Where a block starts.
    ///
    /// A document-level block starts at column 0, indentation included, where comrak starts it at the
    /// first non-space. A nested block starts where comrak says, which is already the right column:
    /// the container prefixes before it are not part of the block. The one exception is an indented
    /// code block, whose own extra indentation is what makes it code, so it starts at the container's
    /// content column instead — see `emit_code_block`.
    fn start_col(&self, node: ComrakNode<'_>, nesting: Nesting) -> (u32, u32) {
        let (row, col) = self.start_of(node);
        (
            row,
            match nesting {
                Nesting::TopLevel => 0,
                _ => col,
            },
        )
    }

    fn build_document(&mut self, root: ComrakNode<'a>, source: &str) -> u32 {
        let document_end = self.document_end();
        let document = self.add(Kind::Document, (0, 0), document_end);

        let plus = synth::plus_front_matter(source, &self.lines);

        // Blocks go into a scratch list first: `section` grouping and list-end clamping both need to
        // see a block's successor before its span can be settled. Front matter is not part of that
        // list — tree-sitter-md hangs it directly off `document`, ahead of every section.
        let mut top_level: Vec<u32> = Vec::new();
        let mut content_start = (0, 0);
        let mut plus_emitted = false;
        for child in root.children() {
            let row = self.start_of(child).0;
            let inside_plus = plus.is_some_and(|span| span.contains_row(row));
            if matches!(child.data().value, NodeValue::FrontMatter(_)) || inside_plus {
                if inside_plus {
                    if plus_emitted {
                        continue;
                    }
                    plus_emitted = true;
                    let span = plus.expect("inside_plus");
                    let node = self.add(Kind::PlusMetadata, span.start(), span.end());
                    self.cover(node);
                    content_start = self.nodes[node as usize].end();
                    self.push(document, node);
                    continue;
                }
                let node = self.emit_leaf(Kind::MinusMetadata, child, Nesting::TopLevel);
                content_start = self.nodes[node as usize].end();
                self.push(document, node);
                continue;
            }
            if self.math_at(child, &mut top_level) {
                continue;
            }
            self.emit_block(child, Nesting::TopLevel, &mut top_level);
        }

        self.clamp_list_ends(&top_level);
        self.fix_indented_code_ends(&top_level);
        self.extend_block_ends();
        self.attach_link_reference_definitions(&mut top_level);
        for child in self.group_sections(&top_level, content_start, document_end) {
            self.push(document, child);
        }
        document
    }

    fn document_end(&self) -> (u32, u32) {
        let last_row = self.lines.line_count() as u32 - 1;
        (last_row, self.lines.content_len(last_row as usize))
    }

    /// comrak ends a list at its last item's last character; tree-sitter-md runs it over trailing
    /// blank lines up to the next block. Rules that ask whether a list is followed by a blank line
    /// (MD031, MD032) depend on the difference, so a list ends where its successor starts.
    fn clamp_list_ends(&mut self, top_level: &[u32]) {
        let document_end = self.document_end();
        self.clamp_siblings(top_level, document_end);
        // Parents are always added before their children, so by the time a node is visited its own
        // end has already been clamped by its parent and can be handed down.
        let count = self.nodes.len();
        for index in 0..count as u32 {
            let children = std::mem::take(&mut self.nodes[index as usize].children);
            let parent_end = self.nodes[index as usize].end();
            self.clamp_siblings(&children, parent_end);
            self.nodes[index as usize].children = children;
        }
    }

    /// Runs a list or list item on over any trailing blank lines, up to whatever follows it.
    fn clamp_siblings(&mut self, siblings: &[u32], parent_end: (u32, u32)) {
        for position in 0..siblings.len() {
            let index = siblings[position];
            if !matches!(self.nodes[index as usize].kind, Kind::List | Kind::ListItem) {
                continue;
            }
            let end = match siblings.get(position + 1) {
                Some(&next) => self.nodes[next as usize].start(),
                None => parent_end,
            };
            if end > self.nodes[index as usize].end() {
                let node = &mut self.nodes[index as usize];
                (node.end_row, node.end_col) = end;
            }
        }
    }

    /// Recomputes where an indented code block ends.
    ///
    /// comrak reports a zero-width sourcepos for an indented code block inside a list item that is
    /// followed by more content in the same item — `3:8-3:8` for a seven-line block — so the range
    /// has to come from the source. That matters well beyond cosmetics: every rule that asks "is
    /// this line inside a code block?" reads this range out of `node_cache`, so MD009 reports the
    /// trailing spaces on each line of a block it was told is one line long.
    ///
    /// The block covers every following row that is blank or indented four past the container's
    /// content column, and then runs on over the trailing blank rows to wherever the next block
    /// starts — or to its parent's end when it is the last child. `extend_block_ends` supplies the
    /// column afterwards.
    fn fix_indented_code_ends(&mut self, top_level: &[u32]) {
        let parents = self.parent_map();
        let count = self.nodes.len();
        let fixed: Vec<(usize, (u32, u32))> = (0..count)
            .filter(|&index| self.nodes[index].kind == Kind::IndentedCodeBlock)
            .map(|index| (index, self.indented_code_end(&parents, top_level, index)))
            .collect();
        for (index, end) in fixed {
            let node = &mut self.nodes[index];
            (node.end_row, node.end_col) = end;
        }
    }

    fn indented_code_end(&self, parents: &[u32], top_level: &[u32], index: usize) -> (u32, u32) {
        let start_row = self.nodes[index].start_row;
        // Four past the container's content column, not the first row's own indentation: a later row
        // only has to reach the minimum, so a block may open at five spaces and continue at four.
        let prefix = continuation_prefix(&self.nodes, parents, &self.lines, index, start_row);
        let indent = prefix + 4;

        let mut last_code = start_row;
        let mut row = start_row + 1;
        while row < self.lines.line_count() as u32 {
            match self.content_indent(parents, index, row) {
                // Blank once container prefixes are stripped, so still inside the block.
                None => row += 1,
                Some(column) if column >= indent => {
                    last_code = row;
                    row += 1;
                }
                Some(_) => break,
            }
        }

        let natural = if self.lines.ends_with_line_terminator() {
            (last_code + 1, 0)
        } else {
            (last_code, self.lines.content_len(last_code as usize))
        };

        // Trailing blank rows belong to the block's range too, so it runs on to whatever follows.
        let parent = parents[index];
        let (siblings, parent_end) = if parent == u32::MAX {
            (top_level, self.document_end())
        } else {
            (
                self.nodes[parent as usize].children.as_slice(),
                self.nodes[parent as usize].end(),
            )
        };
        let followed = siblings
            .iter()
            .position(|&sibling| sibling as usize == index)
            .and_then(|position| siblings.get(position + 1))
            .map(|&next| self.nodes[next as usize].start())
            .unwrap_or(parent_end);
        natural.max(followed)
    }

    /// The column `row`'s first non-space byte sits at, counting only what follows the container
    /// prefixes that still enclose `index`. `None` when nothing does, i.e. the row is blank.
    fn content_indent(&self, parents: &[u32], index: usize, row: u32) -> Option<u32> {
        let text = self.lines.content(row as usize);
        let prefix = continuation_prefix(&self.nodes, parents, &self.lines, index, row) as usize;
        let rest = text[prefix.min(text.len())..].trim_start_matches([' ', '\t']);
        (!rest.is_empty()).then_some((text.len() - rest.len()) as u32)
    }

    /// tree-sitter-md folds a block's continuation prefixes into the block's own range, so inside a
    /// container the block ends where the next line's prefixes stop rather than at column 0. Which
    /// prefixes still apply is decided per container: a block quote continues while the line carries
    /// its `>`, and a list item continues only while the line is indented to the item's content
    /// column. So `- a` followed by `  - b` ends its paragraph at column 2, but `  - a` followed by
    /// `  - b` ends at column 0, because that second line opens a sibling item.
    ///
    /// Containers are not extended here — `clamp_list_ends` settles those against their successors.
    fn extend_block_ends(&mut self) {
        const EXTENDED: &[Kind] = &[
            Kind::Paragraph,
            Kind::AtxHeading,
            Kind::SetextHeading,
            Kind::FencedCodeBlock,
            Kind::IndentedCodeBlock,
            Kind::HtmlBlock,
            Kind::ThematicBreak,
        ];
        let parents = self.parent_map();
        let extensions: Vec<(usize, u32)> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| EXTENDED.contains(&node.kind) && node.end_col == 0)
            .filter(|(_, node)| self.lines.has_line(node.end_row as usize))
            .map(|(index, node)| {
                let col =
                    continuation_prefix(&self.nodes, &parents, &self.lines, index, node.end_row);
                (index, col)
            })
            .filter(|&(_, col)| col > 0)
            .collect();
        for (index, col) in extensions {
            self.nodes[index].end_col = col;
        }
    }

    fn parent_map(&self) -> Vec<u32> {
        let mut parents = vec![u32::MAX; self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate() {
            for &child in &node.children {
                parents[child as usize] = index as u32;
            }
        }
        parents
    }

    /// comrak strips leading `[label]: url` lines out of a paragraph, detaches the paragraph when
    /// nothing is left, and keeps the results in a private map with no positions. MD053 matches
    /// `"link_reference_definition"`, so the nodes are reconstructed here: whatever lines no emitted
    /// block claimed, and that are not blank, were definitions.
    fn attach_link_reference_definitions(&mut self, top_level: &mut Vec<u32>) {
        let runs = synth::reference_definitions(&self.lines, &self.covered);
        for run in runs {
            let node = self.add(Kind::LinkReferenceDefinition, run.start(), run.end());
            self.cover(node);
            let start_row = self.nodes[node as usize].start_row;
            match self.deepest_container(top_level, start_row) {
                Some(parent) => self.insert_in_order(parent, node),
                // Nothing claimed it, so it is a document-level block and has to take part in
                // `section` grouping like any other.
                None => {
                    let position = top_level
                        .iter()
                        .position(|&sibling| self.nodes[sibling as usize].start_row > start_row)
                        .unwrap_or(top_level.len());
                    top_level.insert(position, node);
                }
            }
        }
    }

    /// The innermost already-emitted container spanning `row`, so a definition inside a block quote
    /// or list item attaches there rather than to the document.
    fn deepest_container(&self, candidates: &[u32], row: u32) -> Option<u32> {
        candidates
            .iter()
            .copied()
            .filter(|&index| {
                let node = &self.nodes[index as usize];
                matches!(node.kind, Kind::BlockQuote | Kind::ListItem | Kind::List)
                    && node.start_row <= row
                    && row <= node.last_row()
            })
            .min_by_key(|&index| {
                let node = &self.nodes[index as usize];
                node.last_row() - node.start_row
            })
    }

    /// Definitions are found after their container was emitted, so they are spliced into source
    /// order rather than appended.
    fn insert_in_order(&mut self, parent: u32, node: u32) {
        let start_row = self.nodes[node as usize].start_row;
        let position = {
            let children = &self.nodes[parent as usize].children;
            children
                .iter()
                .position(|&child| self.nodes[child as usize].start_row > start_row)
                .unwrap_or(children.len())
        };
        self.nodes[parent as usize].children.insert(position, node);
    }

    /// tree-sitter-md wraps document-level blocks in `section` nodes: one per ATX heading covering it
    /// and everything until the next heading of the same or a higher level, nested by depth, plus a
    /// leading section for whatever precedes the first heading. MD036 walks that nesting to tell a
    /// document-level paragraph from one buried in a container.
    ///
    /// The leading section exists whenever the range before the first heading is non-empty — even
    /// when it holds no blocks at all, as with a blank line between front matter and a heading.
    fn group_sections(
        &mut self,
        top_level: &[u32],
        content_start: (u32, u32),
        document_end: (u32, u32),
    ) -> Vec<u32> {
        let mut result: Vec<u32> = Vec::new();
        let mut leading: Vec<u32> = Vec::new();
        let mut open: Vec<(u32, u8)> = Vec::new();
        let mut first_heading: Option<(u32, u32)> = None;

        for &block in top_level {
            let Some(level) = self.heading_level(block) else {
                match open.last() {
                    Some(&(section, _)) => self.push(section, block),
                    None => leading.push(block),
                }
                continue;
            };

            let start = self.nodes[block as usize].start();
            first_heading.get_or_insert(start);

            while let Some(&(section, open_level)) = open.last() {
                if open_level < level {
                    break;
                }
                self.close_section(section, start);
                open.pop();
            }
            let section = self.add(Kind::Section, start, document_end);
            self.push(section, block);
            match open.last() {
                Some(&(parent, _)) => self.push(parent, section),
                None => result.push(section),
            }
            open.push((section, level));
        }

        for &(section, _) in open.iter() {
            self.close_section(section, document_end);
        }

        let leading_end = first_heading.unwrap_or(document_end);
        if leading_end > content_start {
            let section = self.add(Kind::Section, content_start, leading_end);
            for &block in &leading {
                self.push(section, block);
            }
            result.insert(0, section);
        }
        result
    }

    fn close_section(&mut self, section: u32, end: (u32, u32)) {
        let node = &mut self.nodes[section as usize];
        (node.end_row, node.end_col) = end;
    }

    /// The level of an ATX heading node, read back from the marker its emitter recorded.
    fn heading_level(&self, block: u32) -> Option<u8> {
        if self.nodes[block as usize].kind != Kind::AtxHeading {
            return None;
        }
        let marker = self.nodes[block as usize].children.first().copied()?;
        Some(match self.nodes[marker as usize].kind {
            Kind::AtxH1Marker => 1,
            Kind::AtxH2Marker => 2,
            Kind::AtxH3Marker => 3,
            Kind::AtxH4Marker => 4,
            Kind::AtxH5Marker => 5,
            _ => 6,
        })
    }

    /// Emits one comrak block node and its descendants, appending to `out`. Emits nothing for the
    /// node kinds the facade does not reproduce.
    fn emit_block(&mut self, node: ComrakNode<'a>, nesting: Nesting, out: &mut Vec<u32>) {
        let data = node.data();
        match &data.value {
            NodeValue::FrontMatter(_) => {
                let index = self.emit_leaf(Kind::MinusMetadata, node, nesting);
                out.push(index);
            }
            NodeValue::BlockQuote => {
                let index = self.emit_container(Kind::BlockQuote, node, nesting);
                out.push(index);
            }
            NodeValue::List(_) => {
                let index = self.emit_container(Kind::List, node, nesting);
                out.push(index);
            }
            NodeValue::Item(list) => {
                let list = *list;
                drop(data);
                let index = self.emit_list_item(node, &list, nesting);
                out.push(index);
            }
            NodeValue::CodeBlock(code) => {
                let (fenced, closed) = (code.fenced, code.closed);
                drop(data);
                let index = self.emit_code_block(node, nesting, fenced, closed);
                out.push(index);
            }
            NodeValue::HtmlBlock(_) => {
                let index = self.emit_leaf(Kind::HtmlBlock, node, nesting);
                out.push(index);
            }
            NodeValue::Paragraph => {
                let forced = self.pending_paragraph_start.take();
                let (start, end) = (
                    forced.unwrap_or_else(|| self.start_col(node, nesting)),
                    self.block_end(node),
                );
                drop(data);
                out.extend(self.emit_paragraph(start, end, Some(node)));
            }
            NodeValue::Heading(heading) => {
                let heading = *heading;
                let (start, end) = (self.start_col(node, nesting), self.block_end(node));
                let underline_row = (node.data().sourcepos.end.line - 1) as u32;
                drop(data);
                let index = self.emit_heading(&heading, start, end, underline_row, node);
                out.push(index);
            }
            NodeValue::ThematicBreak => {
                let index = self.emit_leaf(Kind::ThematicBreak, node, nesting);
                out.push(index);
            }
            NodeValue::Table(_) => {
                let start = self.start_col(node, nesting);
                let end = self.block_end(node);
                let mut rows = Vec::new();
                for child in node.children() {
                    if let NodeValue::TableRow(is_header) = child.data().value {
                        rows.push(((child.data().sourcepos.start.line - 1) as u32, is_header));
                    }
                }
                drop(data);
                let index = self.emit_table(start, end, &rows, node);
                out.push(index);
            }
            // comrak resolves reference definitions into its private refmap and emits nothing here.
            NodeValue::TableRow(_) | NodeValue::TableCell => {}
            // comrak replaces `Item` with `TaskItem` in a task list, and it carries no marker
            // details, so the enclosing list's payload supplies them. tree-sitter-md additionally
            // emits a `task_list_marker_checked`/`_unchecked` sibling; no rule reads it and no rule
            // indexes a list item's children positionally, so it is not reproduced. That is the one
            // intentional node-set divergence from tree-sitter-md.
            NodeValue::TaskItem(_) => {
                let Some(list) = self.enclosing_list else {
                    drop(data);
                    for child in node.children() {
                        self.emit_block(child, nesting, out);
                    }
                    return;
                };
                drop(data);
                let index = self.emit_list_item(node, &list, nesting);
                out.push(index);
            }
            other => {
                let description = format!("{other:?}");
                drop(data);
                debug_assert!(
                    false,
                    "unmapped comrak node {description} — map it or leave its extension flag off"
                );
                let index = self.add(Kind::Unknown, self.start_of(node), self.block_end(node));
                out.push(index);
            }
        }
    }

    fn emit_leaf(&mut self, kind: Kind, node: ComrakNode<'_>, nesting: Nesting) -> u32 {
        let index = self.add(kind, self.start_col(node, nesting), self.block_end(node));
        self.cover(index);
        index
    }

    fn emit_container(&mut self, kind: Kind, node: ComrakNode<'a>, nesting: Nesting) -> u32 {
        let mut start = self.start_col(node, nesting);
        // tree-sitter-md starts a nested container at the enclosing item's content column even when
        // the container's own marker is indented past it, so `1. a` followed by `    * x` puts the
        // inner list — and its first item, and that item's marker — at column 3 rather than 4.
        if let Nesting::Content(column) = nesting {
            start.1 = start.1.min(column);
        }
        let index = self.add(kind, start, self.block_end(node));
        let mut children = Vec::new();

        // A task list's items are `TaskItem`, which carries no marker details, so the enclosing
        // list's payload has to be handed down for the marker to be synthesized at all.
        let saved = self.enclosing_list.take();
        if kind == Kind::List {
            if let NodeValue::List(payload) = &node.data().value {
                self.enclosing_list = Some(*payload);
            }
        }
        let list_start = self.nodes[index as usize].start();
        let child_nesting = match kind {
            // A list's children are its items, which start at their own markers.
            Kind::List => Nesting::OwnMarker,
            // A block quote's children start where its `> ` prefix ends.
            Kind::BlockQuote => Nesting::Content(synth::block_quote_content_col(
                &self.lines,
                list_start.0,
                list_start.1,
            )),
            _ => nesting,
        };
        let mut awaiting_first_item = kind == Kind::List;
        for child in node.children() {
            let is_item = matches!(
                child.data().value,
                NodeValue::Item(_) | NodeValue::TaskItem(_)
            );
            if is_item && awaiting_first_item {
                awaiting_first_item = false;
                self.pending_item_start = Some(list_start);
            }
            if self.math_at(child, &mut children) {
                continue;
            }
            self.emit_block(child, child_nesting, &mut children);
        }
        self.enclosing_list = saved;
        self.pending_item_start = None;

        self.nodes[index as usize].children = children;
        index
    }

    fn emit_list_item(&mut self, node: ComrakNode<'a>, list: &NodeList, nesting: Nesting) -> u32 {
        let comrak_start = self.start_col(node, nesting);
        let start = self.pending_item_start.take().unwrap_or(comrak_start);
        let index = self.add(Kind::ListItem, start, self.block_end(node));

        // The marker runs to the item's content column, which is the marker's own column plus the
        // list's padding — that is how `-   one` gets a four-byte `list_marker_minus`. Padding rather
        // than the first child's column, because in a task list the first child is the paragraph
        // *after* the `[x] ` marker, and the bullet marker still spans only `- `.
        let content_col = comrak_start.1 + list.padding as u32;
        let marker = {
            let lines = &self.lines;
            synth::list_marker(lines, start, Some(content_col), list)
        };
        let marker_node = self.add(marker.kind, marker.span.start(), marker.span.end());

        // The item's content column is the marker's column plus the list's padding — *not* where the
        // marker node ends, because an empty item's marker is clamped to the line and a continuation
        // line still has to reach the full indent. It is also not where a task item's paragraph
        // starts, since `[x] ` sits between them.
        let item_content_col = content_col;
        self.nodes[index as usize].content_col = content_col;

        // GFM only makes `[x]` a task marker when whitespace follows it, so `- [x]` at end of line is
        // an ordinary item whose paragraph starts at the `[` and holds it as literal text. comrak
        // strips the marker either way: it reports the paragraph three columns to the right, and
        // drops it entirely when no later line carries the item's content.
        let bare = matches!(node.data().value, NodeValue::TaskItem(_))
            && self.bare_task_marker(start.0, item_content_col);

        let mut children = Vec::new();
        if bare {
            self.pending_paragraph_start = Some((start.0, item_content_col));
        }
        for child in node.children() {
            if self.math_at(child, &mut children) {
                continue;
            }
            self.emit_block(child, Nesting::Content(item_content_col), &mut children);
        }
        self.pending_paragraph_start = None;
        if bare
            && !children
                .iter()
                .any(|&child| self.nodes[child as usize].kind == Kind::Paragraph)
        {
            // The paragraph is the marker's own line and nothing more: whatever follows belongs to
            // the item's next block, not to the text `[x]`.
            let end = self.lines.block_end_row(start.0);
            let paragraph = self.emit_paragraph((start.0, item_content_col), end, None);
            children.splice(0..0, paragraph);
        }

        self.push(index, marker_node);
        for child in children {
            self.push(index, child);
        }
        index
    }

    /// Whether `child` falls inside a `$$…$$` region, in which case the caller must drop it. The
    /// region's own node is pushed the first time one is reached, so it lands among the siblings the
    /// swallowed blocks would have been; every later block inside the same region is dropped silently.
    ///
    /// A container that *opens* on the region's first line holds the region rather than being
    /// swallowed by it, so the caller descends into it and its own child loop emits the node at the
    /// right depth: `> $$` keeps its block quote and `- $$` keeps its list item. Everything else is
    /// dropped, including a container that merely starts on a later line inside the region.
    ///
    /// Claiming the region's lines also keeps `attach_link_reference_definitions` from reading a
    /// `[a]: /u` inside a math block as a reference definition.
    fn math_at(&mut self, child: ComrakNode<'a>, out: &mut Vec<u32>) -> bool {
        let row = self.start_of(child).0;
        let Some(slot) = self.math.iter().position(|span| span.contains_row(row)) else {
            return false;
        };
        let holds_region = row == self.math[slot].start().0
            && matches!(
                child.data().value,
                NodeValue::BlockQuote
                    | NodeValue::List(_)
                    | NodeValue::Item(_)
                    | NodeValue::TaskItem(_)
            );
        if holds_region {
            return false;
        }
        if !self.math_emitted[slot] {
            self.math_emitted[slot] = true;
            let span = self.math[slot];
            let node = self.add(Kind::MathBlock, span.start(), span.end());
            self.cover(node);
            out.push(node);
        }
        true
    }

    /// Whether the task marker at `column` on `row` is the whole line, which is what stops GFM from
    /// treating it as a marker at all.
    fn bare_task_marker(&self, row: u32, column: u32) -> bool {
        let Some(rest) = self.lines.content(row as usize).get(column as usize..) else {
            return false;
        };
        let marker = rest.trim_end_matches([' ', '\t']);
        marker.len() == 3
            && marker.starts_with('[')
            && marker.ends_with(']')
            && matches!(marker.as_bytes()[1], b' ' | b'x' | b'X')
    }

    fn emit_code_block(
        &mut self,
        node: ComrakNode<'_>,
        nesting: Nesting,
        fenced: bool,
        closed: bool,
    ) -> u32 {
        let kind = if fenced {
            Kind::FencedCodeBlock
        } else {
            Kind::IndentedCodeBlock
        };
        let mut start = self.start_col(node, nesting);
        // The four-plus spaces that make this a code block belong to the container, not to the block.
        if !fenced {
            if let Nesting::Content(column) = nesting {
                start = (start.0, column);
            }
        }
        let index = self.add(kind, start, self.block_end(node));
        self.cover(index);

        if fenced {
            let (start, end) = {
                let emitted = &self.nodes[index as usize];
                (emitted.start(), emitted.end())
            };
            let content = synth::code_fence_content(start.0, start.1, end, closed);
            if let Some(content) = content {
                let node = self.add(Kind::CodeFenceContent, content.start(), content.end());
                self.push(index, node);
            }
        }
        index
    }

    /// A paragraph, or the link reference definitions comrak left at its head plus whatever
    /// paragraph remains. tree-sitter-md emits both for `[a]: /u\nbar`; comrak keeps one paragraph
    /// spanning both lines.
    ///
    /// `source` is comrak's paragraph, whose inline children are hung off the synthesized `inline`
    /// node. It is `None` only for the paragraph this file invents for a bare task marker, which
    /// comrak produced nothing for.
    fn emit_paragraph(
        &mut self,
        start: (u32, u32),
        end: (u32, u32),
        source: Option<ComrakNode<'a>>,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        let last_row = if end.1 == 0 {
            end.0.saturating_sub(1)
        } else {
            end.0
        };

        let mut row = start.0;
        while row <= last_row && synth::is_link_reference_definition(&self.lines, row as usize) {
            let definition_end = self.lines.block_end_row(row);
            let definition = self.add(Kind::LinkReferenceDefinition, (row, 0), definition_end);
            self.cover(definition);
            out.push(definition);
            row += 1;
        }
        if row > last_row {
            return out;
        }

        let paragraph_start = if row == start.0 { start } else { (row, 0) };
        let index = self.add(Kind::Paragraph, paragraph_start, end);
        self.cover(index);
        let inline_end = (last_row, self.lines.inline_end_col(last_row as usize));
        let inline = self.add(Kind::Inline, paragraph_start, inline_end);
        if let Some(source) = source {
            self.emit_inline(inline, source);
        }
        self.push(index, inline);
        out.push(index);
        out
    }

    /// comrak's inline children of a paragraph or heading, hung off the synthesized `inline` node.
    ///
    /// Spans follow the inline convention rather than the block one: comrak's `LineColumn` is 1-based
    /// and end-inclusive, so a start converts to `(line - 1, column - 1)` and an end to
    /// `(line - 1, column)`. An inline end does not swallow a newline. Columns count UTF-8 bytes,
    /// and are absolute — comrak has already skipped the `> ` of a block quote or the indent of a
    /// list item.
    ///
    /// These are in the tree but are deliberately not handed to `RuleLinter::feed` and not put in
    /// `node_cache`; see [`Kind::is_inline`]. Rules opt in by walking, one at a time, so that
    /// switching a kind on cannot make a dead `match` arm fire alongside the regex path it is meant
    /// to replace — md039, md042, md044, md049, md050, md051, md052, md059 and md037 all have both.
    fn emit_inline(&mut self, parent: u32, node: ComrakNode<'a>) {
        for child in node.children() {
            self.emit_inline_node(parent, child);
        }
    }

    fn emit_inline_node(&mut self, parent: u32, node: ComrakNode<'a>) {
        // Taken before `kind` because both borrow the node's value and the borrow has to end before
        // `add` can take `&mut self`.
        let target = match &node.data().value {
            NodeValue::Link(link) => Some(LinkTarget {
                url: link.url.clone(),
                title: link.title.clone(),
            }),
            _ => None,
        };
        let kind = match &node.data().value {
            NodeValue::Text(_) => Kind::Text,
            NodeValue::Code(_) => Kind::CodeSpan,
            NodeValue::Emph => Kind::Emphasis,
            NodeValue::Strong => Kind::StrongEmphasis,
            NodeValue::Link(_) => Kind::Link,
            NodeValue::Image(_) => Kind::Image,
            NodeValue::HtmlInline(_) => Kind::HtmlInline,
            // comrak's math is inline-only and carries its contents as a literal, so this is a leaf.
            NodeValue::Math(_) => Kind::Math,
            // A line break carries no structure a rule can use, and the `text` nodes either side of
            // it already cover the bytes. `Raw` and `EscapedTag` are comrak's text-like leftovers.
            NodeValue::SoftBreak
            | NodeValue::LineBreak
            | NodeValue::Raw(_)
            | NodeValue::EscapedTag(_) => return,
            other => {
                let description = format!("{other:?}");
                debug_assert!(
                    false,
                    "unmapped comrak inline node {description} — map it or leave its extension flag off"
                );
                return;
            }
        };

        let sourcepos = node.data().sourcepos;
        let index = self.add(
            kind,
            (
                (sourcepos.start.line - 1) as u32,
                (sourcepos.start.column - 1) as u32,
            ),
            ((sourcepos.end.line - 1) as u32, sourcepos.end.column as u32),
        );
        if let Some(target) = target {
            self.link_targets.insert(index, target);
        }
        self.emit_inline(index, node);
        self.push(parent, index);
    }

    fn emit_heading(
        &mut self,
        heading: &NodeHeading,
        start: (u32, u32),
        end: (u32, u32),
        underline_row: u32,
        source: ComrakNode<'a>,
    ) -> u32 {
        if heading.setext {
            self.emit_setext_heading(heading, start, end, underline_row, source)
        } else {
            self.emit_atx_heading(start, end, source)
        }
    }

    fn emit_atx_heading(
        &mut self,
        start: (u32, u32),
        end: (u32, u32),
        source: ComrakNode<'a>,
    ) -> u32 {
        let index = self.add(Kind::AtxHeading, start, end);
        self.cover(index);

        let parts = synth::atx_parts(&self.lines, start.0, start.1);
        let marker_node = self.add(
            parts.marker.kind,
            parts.marker.span.start(),
            parts.marker.span.end(),
        );
        self.push(index, marker_node);
        if let Some(span) = parts.inline {
            let inline_node = self.add(Kind::Inline, span.start(), span.end());
            self.emit_inline(inline_node, source);
            self.push(index, inline_node);
        }
        index
    }

    /// tree-sitter-md nests a `paragraph` inside a setext heading, and MD051 walks
    /// `setext_heading -> paragraph -> inline` to reach the heading text.
    fn emit_setext_heading(
        &mut self,
        heading: &NodeHeading,
        start: (u32, u32),
        end: (u32, u32),
        underline_row: u32,
        source: ComrakNode<'a>,
    ) -> u32 {
        let index = self.add(Kind::SetextHeading, start, end);

        let paragraph = self.add(Kind::Paragraph, start, (underline_row, 0));
        let text_row = underline_row.saturating_sub(1);
        let inline = self.add(
            Kind::Inline,
            start,
            (text_row, self.lines.inline_end_col(text_row as usize)),
        );
        self.emit_inline(inline, source);
        self.push(paragraph, inline);
        self.push(index, paragraph);

        let underline = synth::setext_underline(&self.lines, underline_row, heading.level);
        let underline_node = self.add(underline.kind, underline.span.start(), underline.span.end());
        self.push(index, underline_node);

        self.cover(index);
        index
    }

    fn emit_table(
        &mut self,
        start: (u32, u32),
        end: (u32, u32),
        rows: &[(u32, bool)],
        table: ComrakNode<'a>,
    ) -> u32 {
        // Rows and cells come from `synth::table_rows`, not from comrak's `TableCell` children:
        // comrak autocompletes cells to the header width and its cell spans include the surrounding
        // padding, neither of which matches what md055, md056 and md060 measure.
        let index = self.add(Kind::PipeTable, start, end);
        self.cover(index);

        let Some(header) = rows
            .iter()
            .find(|&&(_, is_header)| is_header)
            .map(|&(row, _)| row)
        else {
            return index;
        };
        let body_rows: Vec<u32> = rows
            .iter()
            .filter(|&&(row, is_header)| !is_header && row > header + 1)
            .map(|&(row, _)| row)
            .collect();

        // comrak's cell *geometry* is unusable but its cell *content* is exactly right, so the inline
        // subtree is grafted onto the synthesized cells by byte containment. Without it a cell is a
        // leaf and every rule that scans `inline` is blind inside tables.
        let cell_inlines = self.collect_cell_inlines(table);

        let table_rows = synth::table_rows(&self.lines, start.1, header, &body_rows);
        for row in table_rows {
            let row_node = self.add(row.kind, row.span.start(), row.span.end());
            for child in row.children {
                let child_node = self.add(child.kind, child.span.start(), child.span.end());
                if child.kind == Kind::PipeTableCell {
                    self.emit_cell_inline(child_node, &cell_inlines);
                }
                self.push(row_node, child_node);
            }
            self.push(index, row_node);
        }
        index
    }

    /// One entry per comrak `TableCell` that has content: the byte offset its inline subtree starts
    /// at, the span that subtree covers, and the cell to take the children from. Cells comrak
    /// autocompleted for a short row have no children and are left out.
    fn collect_cell_inlines(&self, table: ComrakNode<'a>) -> Vec<CellInline<'a>> {
        let mut out = Vec::new();
        for row in table.children() {
            if !matches!(row.data().value, NodeValue::TableRow(_)) {
                continue;
            }
            for cell in row.children() {
                if !matches!(cell.data().value, NodeValue::TableCell) {
                    continue;
                }
                let mut span: Option<((u32, u32), (u32, u32))> = None;
                for child in cell.children() {
                    let sourcepos = child.data().sourcepos;
                    let start = (
                        (sourcepos.start.line - 1) as u32,
                        (sourcepos.start.column - 1) as u32,
                    );
                    let end = ((sourcepos.end.line - 1) as u32, sourcepos.end.column as u32);
                    // Children are in source order, so the first one's start and the last one's end
                    // bracket the cell's content.
                    span = Some((span.map_or(start, |(first, _)| first), end));
                }
                if let Some((start, end)) = span {
                    out.push(CellInline {
                        start_byte: self.lines.byte_at(start.0 as usize, start.1 as usize),
                        start,
                        end,
                        source: cell,
                    });
                }
            }
        }
        out
    }

    /// Gives one synthesized `pipe_table_cell` the `inline` subtree comrak parsed for it.
    fn emit_cell_inline(&mut self, cell: u32, inlines: &[CellInline<'a>]) {
        let (cell_start, cell_end) = {
            let node = &self.nodes[cell as usize];
            (
                self.lines
                    .byte_at(node.start_row as usize, node.start_col as usize),
                self.lines
                    .byte_at(node.end_row as usize, node.end_col as usize),
            )
        };
        let Some(found) = inlines.iter().find(|candidate| {
            candidate.start_byte >= cell_start && candidate.start_byte < cell_end
        }) else {
            return;
        };
        let (start, end, source) = (found.start, found.end, found.source);
        let inline = self.add(Kind::Inline, start, end);
        self.emit_inline(inline, source);
        self.push(cell, inline);
    }

    /// Flattens the build-time tree into pre-order storage.
    fn flatten(&mut self, root: u32) -> FacadeTree {
        // Pre-order walk recording each node's build-time index. Children are appended in the same
        // order, so every node's children end up contiguous in `child_index`.
        let mut order: Vec<u32> = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<u32> = vec![root];
        while let Some(index) = stack.pop() {
            order.push(index);
            let children = std::mem::take(&mut self.nodes[index as usize].children);
            for &child in children.iter().rev() {
                stack.push(child);
            }
            self.nodes[index as usize].children = children;
        }

        let mut position_of = vec![0u32; self.nodes.len()];
        for (position, &node_index) in order.iter().enumerate() {
            position_of[node_index as usize] = position as u32;
        }

        let mut nodes = Vec::with_capacity(order.len());
        let mut child_index: Vec<u32> = Vec::with_capacity(order.len());
        for &node_index in &order {
            let children = std::mem::take(&mut self.nodes[node_index as usize].children);
            let children_start = child_index.len() as u32;
            for &child in &children {
                child_index.push(position_of[child as usize]);
            }
            let node = &self.nodes[node_index as usize];
            nodes.push(FacadeNode {
                kind: node.kind,
                start_row: node.start_row,
                start_col: node.start_col,
                end_row: node.end_row,
                end_col: node.end_col,
                start_byte: self
                    .lines
                    .byte_at(node.start_row as usize, node.start_col as usize),
                end_byte: self
                    .lines
                    .byte_at(node.end_row as usize, node.end_col as usize),
                parent: u32::MAX,
                children_start,
                children_len: children.len() as u32,
            });
        }
        // A node's own position is not known until every node is emitted, so parents are patched
        // afterwards from the child slices.
        for parent in 0..nodes.len() {
            let start = nodes[parent].children_start as usize;
            let len = nodes[parent].children_len as usize;
            for &child in &child_index[start..start + len] {
                nodes[child as usize].parent = parent as u32;
            }
        }

        let link_targets = std::mem::take(&mut self.link_targets)
            .into_iter()
            .map(|(index, target)| (position_of[index as usize], target))
            .collect();

        FacadeTree {
            nodes,
            child_index,
            link_targets,
        }
    }
}

/// How many leading columns of `row` are container prefixes still enclosing `paragraph`.
///
/// Walks the paragraph's ancestors outermost-first, accumulating each one's prefix and stopping at
/// the first that the line does not continue.
fn continuation_prefix(
    nodes: &[Node],
    parents: &[u32],
    lines: &LineIndex<'_>,
    paragraph: usize,
    row: u32,
) -> u32 {
    let mut chain = Vec::new();
    let mut current = parents[paragraph];
    while current != u32::MAX {
        chain.push(current as usize);
        current = parents[current as usize];
    }
    chain.reverse();

    let text = lines.content(row as usize).as_bytes();
    let mut col = 0usize;
    for &ancestor in &chain {
        match nodes[ancestor].kind {
            Kind::BlockQuote => {
                // Up to three spaces, then `>`, then one optional space.
                let mut cursor = col;
                let mut spaces = 0;
                while spaces < 3 && text.get(cursor) == Some(&b' ') {
                    cursor += 1;
                    spaces += 1;
                }
                if text.get(cursor) != Some(&b'>') {
                    break;
                }
                cursor += 1;
                if text.get(cursor) == Some(&b' ') {
                    cursor += 1;
                }
                col = cursor;
            }
            Kind::ListItem => {
                let content_col = nodes[ancestor].content_col as usize;
                let mut cursor = col;
                while matches!(text.get(cursor), Some(b' ') | Some(b'\t')) {
                    cursor += 1;
                }
                if cursor < content_col {
                    break;
                }
                col = content_col;
            }
            _ => {}
        }
    }
    col as u32
}
