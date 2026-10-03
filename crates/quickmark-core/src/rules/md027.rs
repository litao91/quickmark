use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{ellipsify, Context, Rule, RuleLinter, RuleType},
};

// MD027-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD027BlockquoteSpacesTable {
    pub list_items: bool,
}

impl Default for MD027BlockquoteSpacesTable {
    fn default() -> Self {
        Self { list_items: true }
    }
}

/// Blocks whose start row ends a code or HTML block's range. Those ranges run on past the block's
/// own last line — over trailing blank rows and over the next block's container prefixes — so a row
/// that opens another block is never code, however the range reads. `block_quote`, `list` and
/// `list_item` count too and are recorded as they are fed.
const BLOCK_STARTS: &[&str] = &[
    "paragraph",
    "atx_heading",
    "setext_heading",
    "thematic_break",
    "pipe_table",
    "link_reference_definition",
    "math_block",
];

/// Lines inside these are literal text, so a `>` on them is not a blockquote symbol.
const CODE_BLOCKS: &[&str] = &["indented_code_block", "fenced_code_block", "html_block"];

/// MD027 Multiple Spaces After Blockquote Symbol Rule Linter
///
/// Reports the whitespace between a `>` and the content it introduces. micromark reaches the same
/// places by asking whether the `linePrefix` token it emitted sits directly after a
/// `blockQuotePrefix`; the equivalent question here is whether anything else claimed the gap, and
/// three things can: a list item's continuation indent, a list that closes on this very line, and
/// a table's delimiter row.
///
/// **SINGLE-USE CONTRACT**: This linter is designed for one-time use only.
/// After processing a document (via feed() calls and finalize()), the linter
/// should be discarded. The violations state is not cleared between uses.
pub(crate) struct MD027Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    /// `(first row, last row, content column)` for every list item.
    items: Vec<(usize, usize, usize)>,
    /// `(row, column)` where a list's range stops. A list that closes does so *after* swallowing
    /// the line's blockquote prefixes, so the gap after them belongs to the list rather than to
    /// the quote.
    list_ends: HashSet<(usize, usize)>,
    block_starts: HashSet<usize>,
    code_rows: Vec<(usize, usize)>,
    delimiter_rows: HashSet<usize>,
    has_block_quote: bool,
}

impl MD027Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            items: Vec::new(),
            list_ends: HashSet::new(),
            block_starts: HashSet::new(),
            code_rows: Vec::new(),
            delimiter_rows: HashSet::new(),
            has_block_quote: false,
        }
    }

    /// The column a list item's content starts at: where its continuation lines have to reach.
    fn item_content_col(item: &Node) -> usize {
        let start_row = item.start_position().row;
        if let Some(content) = item.child(1) {
            if content.start_position().row == start_row {
                return content.start_position().column;
            }
        }
        // An item with nothing on its marker line indents continuation lines one past the marker.
        item.child(0)
            .map_or(item.start_position().column, |marker| {
                marker.end_position().column
            })
    }

    fn last_row(node: &Node) -> usize {
        let end = node.end_position();
        if end.column == 0 {
            end.row.saturating_sub(1)
        } else {
            end.row
        }
    }

    /// The content column of the innermost list item that opened before `row` and still encloses
    /// it, i.e. the column its continuation lines have to be indented to.
    fn enclosing_item(&self, row: usize) -> Option<usize> {
        self.items
            .iter()
            .filter(|&&(start, last, _)| start < row && row <= last)
            .max_by_key(|&&(start, _, content_col)| (start, content_col))
            .map(|&(_, _, content_col)| content_col)
    }

    fn inside_any_item(&self, row: usize) -> bool {
        self.items
            .iter()
            .any(|&(start, last, _)| start <= row && row <= last)
    }

    fn is_literal(&self, row: usize) -> bool {
        self.delimiter_rows.contains(&row)
            || self.code_rows.iter().any(|&(start, end)| {
                start <= row && row <= end && (row == start || !self.block_starts.contains(&row))
            })
    }

    /// Whether the gap at `column` on `row` is the quote's own, i.e. nothing else claimed it.
    fn is_quote_gap(&self, row: usize, column: usize, width: usize, list_items: bool) -> bool {
        if !list_items && self.inside_any_item(row) {
            return false;
        }
        if self.list_ends.contains(&(row, column)) {
            return false;
        }
        if let Some(content_col) = self.enclosing_item(row) {
            // Reaching the item's content column makes this its continuation indent; falling short
            // leaves the paragraph lazily continued, which is still the quote's own gap.
            if column < content_col && column + width >= content_col {
                return false;
            }
        }
        true
    }

    fn analyze_all_lines(&mut self) {
        let list_items = self
            .context
            .config
            .linters
            .settings
            .blockquote_spaces
            .list_items;
        let gaps = {
            let lines = self.context.lines.borrow();
            let mut gaps = Vec::new();

            for (row, line) in lines.iter().enumerate() {
                if self.is_literal(row) {
                    continue;
                }
                let bytes = line.as_bytes();
                let indent = bytes.iter().take_while(|&&b| b == b' ').count();
                // Four past the enclosing item's content column is indented code, and a `>` in code
                // is text — the line opens no blockquote at all.
                let available = self.enclosing_item(row).unwrap_or(0);
                if indent.saturating_sub(available) >= 4 {
                    continue;
                }

                let mut column = indent;
                while column < bytes.len() && bytes[column] == b'>' {
                    column += 1;
                    // One space after the symbol is part of the prefix, so it is never the gap.
                    if bytes.get(column) == Some(&b' ') {
                        column += 1;
                    }
                    let width = bytes[column..].iter().take_while(|&&b| b == b' ').count();
                    if width == 0 {
                        continue;
                    }
                    // markdownlint measures the gap in columns and rejects a range that spans a
                    // tab, so a tab ends the line as far as this rule is concerned.
                    if bytes.get(column + width) == Some(&b'\t') {
                        break;
                    }
                    if self.is_quote_gap(row, column, width, list_items) {
                        gaps.push((row, column, width));
                    }
                    column += width;
                }
            }
            gaps
        };

        for (row, column, width) in gaps {
            self.push(row, column, width);
        }
    }

    fn push(&mut self, row: usize, column: usize, width: usize) {
        let start_byte = self.context.line_start_byte(row) + column;
        // markdownlint quotes the whole line, untrimmed.
        let line = self
            .context
            .lines
            .borrow()
            .get(row)
            .cloned()
            .unwrap_or_default();
        let violation = RuleViolation::new(
            &MD027,
            format!(
                "{} [Context: \"{}\"]",
                MD027.description,
                ellipsify(&line, false, false)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte,
                end_byte: start_byte + width,
                start_point: crate::ast::Point { row, column },
                end_point: crate::ast::Point {
                    row,
                    column: column + width,
                },
            }),
        );
        self.violations.push(violation);
    }
}

impl RuleLinter for MD027Linter {
    fn feed(&mut self, node: &Node) {
        let kind = node.kind();
        match kind {
            "block_quote" => {
                self.has_block_quote = true;
                self.block_starts.insert(node.start_position().row);
            }
            "list" => {
                let end = node.end_position();
                self.list_ends.insert((end.row, end.column));
                self.block_starts.insert(node.start_position().row);
            }
            "list_item" => {
                let start = node.start_position().row;
                self.items
                    .push((start, Self::last_row(node), Self::item_content_col(node)));
                self.block_starts.insert(start);
            }
            "pipe_table_delimiter_row" => {
                self.delimiter_rows.insert(node.start_position().row);
            }
            _ => {
                if CODE_BLOCKS.contains(&kind) {
                    self.code_rows
                        .push((node.start_position().row, node.end_position().row));
                } else if BLOCK_STARTS.contains(&kind) {
                    self.block_starts.insert(node.start_position().row);
                }
            }
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        if self.has_block_quote {
            self.analyze_all_lines();
        }
        std::mem::take(&mut self.violations)
    }
}

pub const MD027: Rule = Rule {
    id: "MD027",
    aliases: &["no-multiple-space-blockquote"],
    tags: &["blockquote", "whitespace", "indentation"],
    description: "Multiple spaces after blockquote symbol",
    rule_type: RuleType::Hybrid,
    // Every node is offered, so the linter collects the block ranges it needs as they pass.
    required_nodes: &[],
    new_linter: |context| Box::new(MD027Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD027BlockquoteSpacesTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::{test_config_with_rules, test_config_with_settings};

    fn config(list_items: bool) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("no-multiple-space-blockquote", RuleSeverity::Error)],
            LintersSettingsTable {
                blockquote_spaces: MD027BlockquoteSpacesTable { list_items },
                ..Default::default()
            },
        )
    }

    /// `(line, column)` of every violation, both 1-based, plus the gap's width.
    fn reported(source: &str, list_items: bool) -> Vec<(usize, usize, usize)> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config(list_items), source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                )
            })
            .collect()
    }

    /// One measured case: whether `list_items` was on, a name for the failure message, the source,
    /// and markdownlint's `(line, column, width)` output.
    type Case = (
        bool,
        &'static str,
        &'static str,
        &'static [(usize, usize, usize)],
    );

    /// Every expectation here is the output of markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) run
    /// with only `no-multiple-space-blockquote` enabled, and `list_items: false` where noted. The
    /// third element of each expectation is markdownlint's `errorRange` length, i.e. the width of
    /// the gap it points at.
    ///
    /// Columns count UTF-8 bytes here and characters there; the two agree on every case below
    /// because each gap is ASCII spaces.
    const CASES: &[Case] = &[
        (
            true,
            "one gap per line, as wide as the gap",
            "> one\n>  two\n>   three\n>    four\n",
            &[(2, 3, 1), (3, 3, 2), (4, 3, 3)],
        ),
        (
            true,
            "a single space, or none, is not a gap",
            "> a\n>b\n> \n>\n",
            &[],
        ),
        (
            true,
            "an empty quote line still has a gap",
            ">\n> \n>  \n>   \n",
            &[(3, 3, 1), (4, 3, 2)],
        ),
        (
            true,
            "each nesting level has its own gap",
            "> > text\n> >  nested\n>  > outer\n>>  tight\n> > >  deep\n>  > >  deep2\n",
            &[
                (2, 5, 1),
                (3, 3, 1),
                (4, 4, 1),
                (5, 7, 1),
                (6, 3, 1),
                (6, 8, 1),
            ],
        ),
        (
            true,
            "one line can carry two gaps",
            ">  >  text\n>   >   text\n> > text\n",
            &[(1, 3, 1), (1, 6, 1), (2, 3, 2), (2, 7, 2)],
        ),
        (
            true,
            "indentation before the first symbol is not a gap, and four spaces makes it code",
            " >  one\n  >   two\n   >    three\n    >     four\n",
            &[(1, 4, 1), (2, 5, 2), (3, 6, 3)],
        ),
        (
            true,
            "a list item's continuation indent belongs to the list",
            "> - item\n>   continuation\n> - two\n>     deeper\n",
            &[],
        ),
        (
            true,
            "nor does a nested list's",
            "> - item\n>   - nested\n>     - deep\n",
            &[],
        ),
        (
            true,
            "an ordered item's continuation indent, over-indented or not",
            "> 1. ordered\n>    continuation\n> 2. second\n>     over\n",
            &[],
        ),
        (
            true,
            "only the first of a run of freshly opened lists is a gap",
            ">   - a\n>   1. b\n>   * c\n>   + d\n",
            &[(1, 3, 2)],
        ),
        (
            true,
            "the same, with markers of mixed kinds",
            ">  - Dash\n>   + Plus\n>    * Star\n>     1. Ordered\n>      2) Paren\n",
            &[(1, 3, 1), (4, 3, 4)],
        ),
        (
            true,
            "a marker that matches the open list keeps it open, so its gap counts",
            "> - item\n>  - another\n",
            &[(2, 3, 1)],
        ),
        (
            true,
            "and one level deeper",
            "> - item\n>   - nested\n>  - back\n",
            &[(3, 3, 1)],
        ),
        (
            true,
            "deep nesting indents all the way down",
            "> - a\n>   - b\n>     - c\n>       - d\n",
            &[],
        ),
        (
            true,
            "indented code inside a quote is literal, the lines after it are not",
            ">     code\n>   not code\n>    also not\n",
            &[(2, 3, 2), (3, 3, 3)],
        ),
        (
            true,
            "four spaces that continue a paragraph are not code",
            "> text\n>     more\n>   and more\n",
            &[(2, 3, 4), (3, 3, 2)],
        ),
        (
            true,
            "four spaces after code that a list item swallows",
            ">     code\n>\n>\n>  text\n",
            &[(4, 3, 1)],
        ),
        (
            true,
            "the line after a code block is not part of it",
            ">     code\n>  text\n",
            &[(2, 3, 1)],
        ),
        (
            true,
            "a fenced block is literal through its closing fence",
            "> ```\n>  x\n> ```\n>  after\n",
            &[(4, 3, 1)],
        ),
        (
            true,
            "an unclosed fence runs to the end of the file",
            "> ```\n>   x",
            &[],
        ),
        (
            true,
            "a fence inside a list item",
            "> - item\n>\n>   ```\n>    x\n>   ```\n>   text\n",
            &[],
        ),
        (
            true,
            "an HTML block is literal through its end",
            "> <!--\n>  x\n> -->\n>  after\n",
            &[(4, 3, 1)],
        ),
        (
            true,
            "an unclosed HTML block runs to the end of the file",
            "> <!--\n>  x\n",
            &[],
        ),
        (
            true,
            "inline HTML is not a block",
            ">  a <b>  c\n",
            &[(1, 3, 1)],
        ),
        (
            true,
            "a quote inside a list, indented by two",
            "- item\n\n  >   quote\n  > quote\n",
            &[(3, 5, 2)],
        ),
        (
            true,
            "a quote inside a list, indented by four",
            "- item\n\n    >   quote\n    > quote\n",
            &[(3, 7, 2)],
        ),
        (
            true,
            "four spaces of file indentation before a symbol opens no quote",
            "text\n\n     >  x\n",
            &[],
        ),
        (
            true,
            "but four past a list item's content column still does",
            "text\n\n    > x\n\n>  y\n",
            &[(5, 3, 1)],
        ),
        (
            true,
            "a loose list's continuation indent, then a nested quote's own gap",
            "> - item\n>\n>   >   quote\n",
            &[(3, 7, 2)],
        ),
        (
            true,
            "the same with a single extra space",
            "> - item\n>\n>   >  quote\n",
            &[(3, 7, 1)],
        ),
        (
            true,
            "and with the nested quote opening on the item's own line",
            "> - item\n> >  quote\n",
            &[(2, 5, 1)],
        ),
        (
            true,
            "a quote that ends and a new one that starts",
            "> - item\n>\n>   > nested\n>   >  extra\n",
            &[(4, 7, 1)],
        ),
        (
            true,
            "a quote line with nothing on it",
            "> text\n>\n>   more\n",
            &[(3, 3, 2)],
        ),
        (
            true,
            "headings and setext underlines are ordinary content",
            ">  # heading\n> text\n>   more\n",
            &[(1, 3, 1), (3, 3, 2)],
        ),
        (
            true,
            "a setext heading inside a quote",
            ">  Heading\n>  ========\n>   more\n",
            &[(1, 3, 1), (2, 3, 1), (3, 3, 2)],
        ),
        (
            true,
            "a thematic break inside a quote",
            ">  ---\n>   text\n",
            &[(1, 3, 1), (2, 3, 2)],
        ),
        (
            true,
            "a paragraph then a list, both with a gap",
            ">   text\n>   - list after\n",
            &[(1, 3, 2), (2, 3, 2)],
        ),
        (
            true,
            "markers with no space after them are prose",
            ">  -No space\n>   +No space\n>    1.No space\n",
            &[(1, 3, 1), (2, 3, 2), (3, 3, 3)],
        ),
        (
            true,
            "digits and dots that are not a marker either",
            ">  1.Item\n>   2. Item\n>    10. Double\n>     100. Triple\n",
            &[(1, 3, 1), (2, 3, 2), (3, 3, 3), (4, 3, 4)],
        ),
        (
            true,
            "five spaces after a marker pull the content column back to one",
            "> -     five spaces\n>   continuation\n",
            &[],
        ),
        (
            true,
            "an empty item indents its continuation one past the marker",
            "> -\n>   text\n",
            &[],
        ),
        (
            true,
            "an empty item with trailing spaces",
            "> -  \n>   text\n",
            &[],
        ),
        (
            true,
            "a lazy continuation line carries no symbol at all",
            "> - item\n  lazy\n>   - nested\n",
            &[],
        ),
        (
            true,
            "a table's delimiter row is structure, its other rows are not",
            ">  | a | b |\n>  |---|---|\n>   | c | d |\n",
            &[(1, 3, 1), (3, 3, 2)],
        ),
        (
            true,
            "quotes at different depths on neighbouring lines",
            ">  a\n> > b\n>  c\n",
            &[(1, 3, 1), (3, 3, 1)],
        ),
        (
            true,
            "a gap before text that is not ASCII",
            ">  你好\n>   こんにちは\n",
            &[(1, 3, 1), (2, 3, 2)],
        ),
        (
            true,
            "a file with no trailing newline",
            ">  a\n>   b",
            &[(1, 3, 1), (2, 3, 2)],
        ),
        (
            true,
            "CRLF line endings",
            ">  a\r\n>   b\r\n> c\r\n",
            &[(1, 3, 1), (2, 3, 2)],
        ),
        (
            true,
            "CRLF line endings inside a list",
            "> - item\r\n>   cont\r\n>  x\r\n",
            &[(3, 3, 1)],
        ),
        (
            true,
            "a tab after the gap makes markdownlint throw, so nothing is reported",
            ">\ttext\n> \tx\n>\tx\n>  \ty\n",
            &[],
        ),
        (
            false,
            "list_items off: nothing inside a list item",
            ">   - fresh list\n>   text\n",
            &[],
        ),
        (
            false,
            "list_items off: a two-space gap before a list",
            ">  - two spaces list\n>  text\n",
            &[],
        ),
        (
            false,
            "list_items off: an ordered list",
            ">   1. ordered fresh\n>   prose\n",
            &[],
        ),
        (
            false,
            "list_items off: prose before the list still counts",
            ">   text\n>   - list after\n",
            &[(1, 3, 2)],
        ),
        (
            false,
            "list_items off: a plain continuation",
            "> - item\n>   continuation\n",
            &[],
        ),
        (
            false,
            "list_items off: no list, so no exemption",
            ">  1.Item no space\n>   text\n",
            &[(1, 3, 1), (2, 3, 2)],
        ),
        (
            false,
            "list_items off: mixed markers all sit inside items",
            ">  - Dash\n>   + Plus\n>    * Star\n>     1. Ordered\n>      2) Paren\n",
            &[],
        ),
        (
            false,
            "list_items off: malformed markers are prose",
            ">  -No space\n>   +No space\n>    *No space\n>     1.No space\n",
            &[(1, 3, 1), (2, 3, 2), (3, 3, 3), (4, 3, 4)],
        ),
        (
            false,
            "list_items off: digits and dots that are not a marker",
            ">  1.Item\n>   2. Item\n>    10. Double\n>     100. Triple\n",
            &[(1, 3, 1), (2, 3, 2), (3, 3, 3), (4, 3, 4)],
        ),
        (
            false,
            "list_items off: a quote inside a list item",
            "- item\n\n  >   quote\n",
            &[],
        ),
        (
            false,
            "list_items off: an item whose sub-list is prose",
            ">  1. Item\n>     a. Sub\n",
            &[],
        ),
        (
            false,
            "list_items off: no list at all",
            "> one\n>  two\n",
            &[(2, 3, 1)],
        ),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(list_items, name, source, expected) in CASES {
            assert_eq!(
                expected,
                reported(source, list_items).as_slice(),
                "case `{name}` with list_items={list_items}"
            );
        }
    }

    #[test]
    fn a_long_quote_reports_every_other_line() {
        let source: String = (0..1000)
            .map(|index| {
                if index % 2 == 0 {
                    format!(">  Line {index} with a gap\n")
                } else {
                    format!("> Line {index} without\n")
                }
            })
            .collect();
        assert_eq!(500, reported(&source, true).len());
    }

    #[test]
    fn deeply_nested_quotes_still_report() {
        let source: String = (0..100)
            .map(|depth| {
                let symbols = ">".repeat(depth + 1);
                if depth % 10 == 0 {
                    format!("{symbols}  Line {depth}\n")
                } else {
                    format!("{symbols} Line {depth}\n")
                }
            })
            .collect();
        assert_eq!(10, reported(&source, true).len());
    }

    #[test]
    fn a_document_without_quotes_is_not_scanned() {
        assert_eq!(0, reported("no quotes here\n  just text\n", true).len());
    }

    #[test]
    fn list_items_defaults_to_true() {
        let config =
            test_config_with_rules(vec![("no-multiple-space-blockquote", RuleSeverity::Error)]);
        assert!(config.linters.settings.blockquote_spaces.list_items);
        assert_eq!(
            crate::config::MD027BlockquoteSpacesTable::default(),
            config.linters.settings.blockquote_spaces
        );
    }
}
