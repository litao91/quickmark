//! The document tree that quickmark's rules walk.
//!
//! The tree is built from comrak's AST but deliberately does not expose it. Rules were written
//! against `tree_sitter::Node`, and two of its conventions are load-bearing across all 53 of them,
//! so this module reproduces both exactly:
//!
//! - **Columns are UTF-8 byte offsets, not character counts.** comrak counts bytes too, but only
//!   when `parse.sourcepos_chars` is left off.
//! - **A block's end position swallows its trailing newline.** tree-sitter-md's grammar states it
//!   outright ("All blocks contain a trailing newline"); comrak's end is the block's last character.
//!   Every rule that asks "is the line after this block blank?" depends on the difference, so
//!   [`block_end`] normalizes it.
//!
//! Block kinds are the names tree-sitter-md's block grammar produced, because that is what 53 rule
//! files match on. Inline kinds come from comrak and are in the tree, but they are neither fed to
//! rules nor cached — see [`Kind::is_inline`] for why, and [`KIND_NAMES_FORBIDDEN`] for the spellings
//! that must never appear at all.

pub mod build;
pub mod walker;

pub(crate) mod synth;

#[cfg(test)]
mod snapshot;

/// Every node kind the tree can contain. The discriminant indexes [`KIND_NAMES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Document,
    Section,
    MinusMetadata,
    PlusMetadata,
    AtxHeading,
    SetextHeading,
    AtxH1Marker,
    AtxH2Marker,
    AtxH3Marker,
    AtxH4Marker,
    AtxH5Marker,
    AtxH6Marker,
    SetextH1Underline,
    SetextH2Underline,
    Paragraph,
    Inline,
    Text,
    CodeSpan,
    Emphasis,
    StrongEmphasis,
    Link,
    Image,
    HtmlInline,
    FencedCodeBlock,
    IndentedCodeBlock,
    CodeFenceContent,
    HtmlBlock,
    MathBlock,
    ThematicBreak,
    BlockQuote,
    List,
    ListItem,
    ListMarkerMinus,
    ListMarkerPlus,
    ListMarkerStar,
    ListMarkerDot,
    ListMarkerParenthesis,
    PipeTable,
    PipeTableHeader,
    PipeTableDelimiterRow,
    PipeTableRow,
    PipeTableCell,
    PipeTableDelimiterCell,
    /// The anonymous `|` between and around table cells. The only unnamed kind.
    Pipe,
    LinkReferenceDefinition,
    /// A comrak node with no tree-sitter-md counterpart. Unreachable while the extension flags in
    /// [`build::comrak_options`] stay off; it exists so flipping one fails loudly instead of
    /// silently dropping content.
    Unknown,
}

/// Kind names, indexed by `Kind as usize`. These strings are what rules match on.
pub const KIND_NAMES: &[&str] = &[
    "document",
    "section",
    "minus_metadata",
    "plus_metadata",
    "atx_heading",
    "setext_heading",
    "atx_h1_marker",
    "atx_h2_marker",
    "atx_h3_marker",
    "atx_h4_marker",
    "atx_h5_marker",
    "atx_h6_marker",
    "setext_h1_underline",
    "setext_h2_underline",
    "paragraph",
    "inline",
    "text",
    "code_span",
    "emphasis",
    "strong_emphasis",
    "link",
    "image",
    "html_inline",
    "fenced_code_block",
    "indented_code_block",
    "code_fence_content",
    "html_block",
    "math_block",
    "thematic_break",
    "block_quote",
    "list",
    "list_item",
    "list_marker_minus",
    "list_marker_plus",
    "list_marker_star",
    "list_marker_dot",
    "list_marker_parenthesis",
    "pipe_table",
    "pipe_table_header",
    "pipe_table_delimiter_row",
    "pipe_table_row",
    "pipe_table_cell",
    "pipe_table_delimiter_cell",
    "|",
    "link_reference_definition",
    "UNKNOWN",
];

/// Kinds that must stay out of [`KIND_NAMES`].
///
/// No parser produces these. They appear in rule `match` arms as spellings of a kind that does not
/// exist — `md059` asks for `"html_tag"` and `"inline_html"` where the real kind is `html_inline`,
/// `md041` asks for `"html_flow"` where the tree emits `html_block`, `md013` asks for `"table"` and
/// `"table_row"` where the tree emits `pipe_table` and `pipe_table_row`. Those arms have never
/// executed and must not be "fixed" into existence by a well-meaning facade; the rules need fixing
/// instead.
pub const KIND_NAMES_FORBIDDEN: &[&str] = &[
    "label",
    "html_tag",
    "html_flow",
    "blockquote",
    "code_block",
    "table",
    "table_row",
    "inline_html",
];

impl Kind {
    pub fn name(self) -> &'static str {
        KIND_NAMES[self as usize]
    }

    /// False only for [`Kind::Pipe`], matching tree-sitter's named/anonymous split.
    pub fn is_named(self) -> bool {
        self != Kind::Pipe
    }

    /// Whether this kind lives under an `inline` node.
    ///
    /// Inline nodes are in the tree but are neither fed to rules nor cached — see
    /// [`crate::linter::MultiRuleLinter`]. Nine rules have both a dead `match` arm on an inline kind
    /// and a live regex path over the enclosing `inline` text, so feeding them would report every
    /// violation twice. A rule opts in by walking into `inline` itself and deleting its regex path
    /// in the same change.
    pub fn is_inline(self) -> bool {
        matches!(
            self,
            Kind::Text
                | Kind::CodeSpan
                | Kind::Emphasis
                | Kind::StrongEmphasis
                | Kind::Link
                | Kind::Image
                | Kind::HtmlInline
        )
    }
}

/// A position in the document: a 0-based `row` and a 0-based `column` counted in UTF-8 bytes.
/// Field names and shape mirror `tree_sitter::Point` so rule code is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// Mirrors `tree_sitter::Range`. Rules construct this literally in about a dozen places, so the
/// field names are part of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: Point,
    pub end_point: Point,
}

const NO_PARENT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FacadeNode {
    kind: Kind,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
    start_byte: u32,
    end_byte: u32,
    parent: u32,
    /// Index into [`FacadeTree::child_index`], whose entries are node indices. Children are always
    /// contiguous because the builder emits pre-order.
    children_start: u32,
    children_len: u32,
}

/// A parsed document. Build one with [`build::parse`].
///
/// Nodes are stored flat in pre-order, so whole-tree traversal is a loop over indices with no
/// pointer chasing, and a deeply nested document cannot overflow the stack — the tree-sitter-based
/// cache builder it replaces recursed once per node.
#[derive(Debug)]
pub struct FacadeTree {
    pub(crate) nodes: Vec<FacadeNode>,
    /// One entry per node, holding node indices. Every node's children occupy a contiguous slice
    /// because the builder emits in pre-order.
    pub(crate) child_index: Vec<u32>,
}

impl FacadeTree {
    pub fn root_node(&self) -> Node<'_> {
        Node {
            tree: self,
            index: 0,
        }
    }

    pub fn node(&self, index: u32) -> Node<'_> {
        Node { tree: self, index }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn get(&self, index: u32) -> &FacadeNode {
        &self.nodes[index as usize]
    }

    pub(crate) fn children_of(&self, index: u32) -> &[u32] {
        let node = self.get(index);
        let start = node.children_start as usize;
        &self.child_index[start..start + node.children_len as usize]
    }
}

/// A handle to one node. `Copy`, because rules stash these in `Option<Node>` and compare them.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    tree: &'a FacadeTree,
    index: u32,
}

impl<'a> Node<'a> {
    pub fn id(&self) -> usize {
        self.index as usize
    }

    pub fn kind(self) -> &'static str {
        self.node().kind.name()
    }

    pub fn is_named(self) -> bool {
        self.node().kind.is_named()
    }

    /// Whether this node lives under an `inline`. Such nodes are in the tree but are neither fed to
    /// rules nor cached; see [`Kind::is_inline`].
    pub fn is_inline(self) -> bool {
        self.node().kind.is_inline()
    }

    pub fn start_position(self) -> Point {
        let n = self.node();
        Point {
            row: n.start_row as usize,
            column: n.start_col as usize,
        }
    }

    pub fn end_position(self) -> Point {
        let n = self.node();
        Point {
            row: n.end_row as usize,
            column: n.end_col as usize,
        }
    }

    pub fn start_byte(self) -> usize {
        self.node().start_byte as usize
    }

    pub fn end_byte(self) -> usize {
        self.node().end_byte as usize
    }

    pub fn range(self) -> NodeRange {
        let n = self.node();
        NodeRange {
            start_byte: n.start_byte as usize,
            end_byte: n.end_byte as usize,
            start_point: Point {
                row: n.start_row as usize,
                column: n.start_col as usize,
            },
            end_point: Point {
                row: n.end_row as usize,
                column: n.end_col as usize,
            },
        }
    }

    /// The node's source text. Takes the document bytes so call sites are unchanged from the
    /// tree-sitter API, and ties the result's lifetime to those bytes rather than to the tree —
    /// rules slice a `RefCell` borrow of the document that does not outlive it.
    pub fn utf8_text(self, source: &[u8]) -> Result<&str, std::str::Utf8Error> {
        let n = self.node();
        std::str::from_utf8(&source[n.start_byte as usize..n.end_byte as usize])
    }

    pub fn parent(self) -> Option<Node<'a>> {
        let parent = self.node().parent;
        (parent != NO_PARENT).then_some(Node {
            tree: self.tree,
            index: parent,
        })
    }

    pub fn child(self, index: usize) -> Option<Node<'a>> {
        self.tree
            .children_of(self.index)
            .get(index)
            .map(|&child| Node {
                tree: self.tree,
                index: child,
            })
    }

    pub fn child_count(self) -> usize {
        self.tree.children_of(self.index).len()
    }

    /// The `index`-th named child, skipping anonymous [`Kind::Pipe`] nodes.
    pub fn named_child(self, index: usize) -> Option<Node<'a>> {
        self.tree
            .children_of(self.index)
            .iter()
            .map(|&child| Node {
                tree: self.tree,
                index: child,
            })
            .filter(|node| node.is_named())
            .nth(index)
    }

    pub fn children(self, _cursor: &mut Cursor<'a>) -> Children<'a> {
        Children {
            tree: self.tree,
            slice: self.tree.children_of(self.index),
            position: 0,
        }
    }

    pub fn walk(self) -> Cursor<'a> {
        Cursor {
            tree: self.tree,
            index: self.index,
        }
    }

    fn node(self) -> &'a FacadeNode {
        self.tree.get(self.index)
    }
}

/// Iterator over a node's children, returned by [`Node::children`].
///
/// tree-sitter's version borrows a cursor; this one ignores it and walks the child slice directly,
/// which is equivalent and lets `node.children(&mut node.walk())` keep compiling.
pub struct Children<'a> {
    tree: &'a FacadeTree,
    slice: &'a [u32],
    position: usize,
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let index = *self.slice.get(self.position)?;
        self.position += 1;
        Some(Node {
            tree: self.tree,
            index,
        })
    }
}

/// Cursor over a node and its relatives, mirroring `tree_sitter::TreeCursor` for the four methods
/// rules actually call.
pub struct Cursor<'a> {
    tree: &'a FacadeTree,
    index: u32,
}

impl<'a> Cursor<'a> {
    pub fn node(&self) -> Node<'a> {
        Node {
            tree: self.tree,
            index: self.index,
        }
    }

    pub fn goto_first_child(&mut self) -> bool {
        match self.tree.children_of(self.index).first() {
            Some(&child) => {
                self.index = child;
                true
            }
            None => false,
        }
    }

    pub fn goto_next_sibling(&mut self) -> bool {
        self.goto_sibling(1)
    }

    pub fn goto_previous_sibling(&mut self) -> bool {
        self.goto_sibling(-1)
    }

    pub fn goto_parent(&mut self) -> bool {
        let parent = self.tree.get(self.index).parent;
        if parent == NO_PARENT {
            return false;
        }
        self.index = parent;
        true
    }

    fn goto_sibling(&mut self, delta: isize) -> bool {
        let parent = self.tree.get(self.index).parent;
        if parent == NO_PARENT {
            return false;
        }
        let siblings = self.tree.children_of(parent);
        let slot = siblings.iter().position(|&child| child == self.index);
        match slot.and_then(|slot| slot.checked_add_signed(delta)) {
            Some(slot) => match siblings.get(slot) {
                Some(&sibling) => {
                    self.index = sibling;
                    true
                }
                None => false,
            },
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the invariant in [`KIND_NAMES_FORBIDDEN`]'s doc comment: no kind a rule misspells may
    /// be emitted, or the dead branch matching it comes alive and silently changes what the rule
    /// reports.
    #[test]
    fn facade_emits_no_forbidden_kinds() {
        for forbidden in KIND_NAMES_FORBIDDEN {
            assert!(
                !KIND_NAMES.contains(forbidden),
                "{forbidden} must not be emitted while a rule still matches that spelling"
            );
        }
    }

    /// `Kind::is_inline` is what keeps inline nodes out of `feed` and `node_cache`, so a kind the
    /// builder emits under `inline` but forgets to list there would be handed to every rule on every
    /// document — which is exactly the double report this design exists to avoid.
    #[test]
    fn inline_is_exactly_the_subtree_under_an_inline_node() {
        let source = "text *em* **strong** `code` [link](/u) ![img](/i) <b>html</b>\n";
        let tree = build::parse(source);

        let mut seen: Vec<&'static str> = Vec::new();
        for index in 0..tree.node_count() {
            let node = tree.node(index as u32);
            let under_inline = node
                .parent()
                .is_some_and(|parent| parent.kind() == "inline" || parent.is_inline());
            assert_eq!(
                node.is_inline(),
                under_inline,
                "{}: is_inline disagrees with its position in the tree",
                node.kind()
            );
            if node.is_inline() {
                seen.push(node.kind());
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen,
            [
                "code_span",
                "emphasis",
                "html_inline",
                "image",
                "link",
                "strong_emphasis",
                "text"
            ],
            "the builder emitted a different set of inline kinds than this test pins"
        );
    }

    #[test]
    fn kind_names_cover_every_variant() {
        assert_eq!(KIND_NAMES.len(), Kind::Unknown as usize + 1);
        assert_eq!(Kind::Document.name(), "document");
        assert_eq!(Kind::Pipe.name(), "|");
        assert!(!Kind::Pipe.is_named());
        assert!(Kind::Paragraph.is_named());
    }
}
