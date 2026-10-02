use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleViolation},
    rules::{Rule, RuleLinter, RuleType},
};

/// Whether the byte at `pos` is escaped, i.e. preceded by an odd number of backslashes. An escaped
/// marker is literal text rather than a delimiter, so `\* a \*` is not emphasis with spaces inside
/// it and markdownlint does not report it.
pub(crate) fn is_escaped(text: &str, pos: usize) -> bool {
    escaped_at(text.as_bytes(), pos)
}

fn escaped_at(bytes: &[u8], pos: usize) -> bool {
    let mut escaped = false;
    let mut index = pos;
    while index > 0 && bytes[index - 1] == b'\\' {
        escaped = !escaped;
        index -= 1;
    }
    escaped
}

/// A run of `*` or `_` that no emphasis claimed. micromark leaves those as bare `data` tokens and
/// MD037 pairs them up two at a time; comrak folds them into the surrounding `text`, so they are
/// recovered here by splitting each text node on its marker runs.
#[derive(Clone, Copy)]
struct Marker {
    /// Byte offset of the run's first character, and just past its last.
    start: usize,
    end: usize,
    symbol: u8,
    /// Length of the run. micromark keeps a longer run as one `data` token, which MD037 ignores.
    width: usize,
}

/// The marker strings MD037 collects, in markdownlint's own order.
const MARKERS: [(u8, usize); 6] = [
    (b'_', 1),
    (b'_', 2),
    (b'_', 3),
    (b'*', 1),
    (b'*', 2),
    (b'*', 3),
];

/// Inline nodes to stay out of. micromark does not split `data` at delimiter runs inside a link or
/// image label, so a marker there is invisible to MD037: `[a * b * c](x)` reports nothing.
const OPAQUE: &[&str] = &["link", "image"];

/// A gap to report: `width` bytes at `column` on `row`, with markdownlint's context string.
struct Gap {
    row: usize,
    column: usize,
    width: usize,
    context: String,
}

fn marker_string(symbol: u8, width: usize) -> String {
    std::iter::repeat_n(symbol as char, width).collect()
}

pub(crate) struct MD037Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD037Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Finds the gaps in one inline subtree.
    ///
    /// markdownlint collects bare markers per micromark token — resetting between tokens, so a
    /// marker never pairs across one — and the equivalent boundary here is the node that owns the
    /// `text` children. `*a * b*` therefore reports nothing: its lone inner marker has no partner
    /// in the same scope.
    fn check_inline(&mut self, inline: &Node) {
        let context = Rc::clone(&self.context);
        let source = context.document_content.borrow();
        let lines = context.lines.borrow();
        let bytes = source.as_bytes();

        let mut gaps = Vec::new();
        for scope in scopes(*inline) {
            let markers = bare_markers(&scope, bytes);
            for (symbol, width) in MARKERS {
                let matching: Vec<&Marker> = markers
                    .iter()
                    .filter(|marker| marker.symbol == symbol && marker.width == width)
                    .collect();
                // Pairs are (0,1), (2,3), … and a trailing odd marker is dropped, as in markdownlint.
                for pair in matching.chunks(2) {
                    if pair.len() != 2 {
                        continue;
                    }
                    gaps.extend(opening_gap(pair[0], symbol, width, &lines, &context));
                    gaps.extend(closing_gap(pair[1], symbol, width, &lines, &context));
                }
            }
        }

        drop(lines);
        drop(source);
        gaps.sort_by_key(|gap| (gap.row, gap.column));
        for gap in gaps {
            self.push(gap);
        }
    }

    fn push(&mut self, gap: Gap) {
        let start_byte = self.context.line_start_byte(gap.row) + gap.column;
        let range = crate::ast::NodeRange {
            start_byte,
            end_byte: start_byte + gap.width,
            start_point: crate::ast::Point {
                row: gap.row,
                column: gap.column,
            },
            end_point: crate::ast::Point {
                row: gap.row,
                column: gap.column + gap.width,
            },
        };
        self.violations.push(RuleViolation::new(
            &MD037,
            format!("{} [Context: \"{}\"]", MD037.description, gap.context),
            self.context.file_path.clone(),
            range_from_node_range(&range),
        ));
    }
}

/// The `text` children of every node in `inline`'s subtree that has any, in document order.
fn scopes(inline: Node) -> Vec<Vec<Node>> {
    let mut out = Vec::new();
    let mut stack = vec![inline];
    while let Some(node) = stack.pop() {
        let mut texts = Vec::new();
        for index in (0..node.child_count()).rev() {
            let Some(child) = node.child(index) else {
                continue;
            };
            if child.kind() == "text" {
                texts.push(child);
            } else if !OPAQUE.contains(&child.kind()) {
                // Pushed in reverse so the stack pops children left to right.
                stack.push(child);
            }
        }
        if !texts.is_empty() {
            texts.reverse();
            out.push(texts);
        }
    }
    out
}

fn bare_markers(texts: &[Node], source: &[u8]) -> Vec<Marker> {
    let mut markers = Vec::new();
    for text in texts {
        let end_of_node = text.end_byte();
        let mut index = text.start_byte();
        while index < end_of_node {
            let symbol = source[index];
            if symbol != b'*' && symbol != b'_' {
                index += 1;
                continue;
            }
            let start = index;
            while index < end_of_node && source[index] == symbol {
                index += 1;
            }
            if index - start <= 3 && !escaped_at(source, start) {
                markers.push(Marker {
                    start,
                    end: index,
                    symbol,
                    width: index - start,
                });
            }
        }
    }
    markers
}

/// The whitespace directly after an opening marker, when a non-whitespace character follows it on
/// the same line. markdownlint's check is `/^\s+\S/` over the rest of the line.
fn opening_gap(
    marker: &Marker,
    symbol: u8,
    width: usize,
    lines: &[String],
    context: &Context,
) -> Option<Gap> {
    let point = context.point_at(marker.end);
    let tail = lines.get(point.row)?.get(point.column..)?;
    let gap = tail
        .char_indices()
        .find(|&(_, character)| !character.is_whitespace())
        .map_or(tail.len(), |(index, _)| index);
    if gap == 0 {
        return None;
    }
    let next = tail[gap..].chars().next()?;
    Some(Gap {
        row: point.row,
        column: point.column,
        width: gap,
        context: format!("{}{}{}", marker_string(symbol, width), &tail[..gap], next),
    })
}

/// The whitespace directly before a closing marker, when a non-whitespace character precedes it on
/// the same line. markdownlint's check is `/\S\s+$/` over everything before the marker.
fn closing_gap(
    marker: &Marker,
    symbol: u8,
    width: usize,
    lines: &[String],
    context: &Context,
) -> Option<Gap> {
    let point = context.point_at(marker.start);
    let head = lines.get(point.row)?.get(..point.column)?;
    let trimmed = head.trim_end();
    if trimmed.len() == head.len() {
        return None;
    }
    let previous = trimmed.chars().next_back()?;
    let whitespace = &head[trimmed.len()..];
    Some(Gap {
        row: point.row,
        column: point.column - whitespace.len(),
        width: whitespace.len(),
        context: format!("{}{}{}", previous, whitespace, marker_string(symbol, width)),
    })
}

impl RuleLinter for MD037Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.check_inline(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD037: Rule = Rule {
    id: "MD037",
    alias: "no-space-in-emphasis",
    tags: &["whitespace", "emphasis"],
    description: "Spaces inside emphasis markers",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD037Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// One gap: `(line, column, width, context)`, the first three 1-based.
    type Gap<'a> = (usize, usize, usize, &'a str);

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-space-in-emphasis", RuleSeverity::Error)])
    }

    fn gaps(source: &str) -> Vec<(usize, usize, usize, String)> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                let context = violation
                    .message()
                    .split_once("[Context: \"")
                    .map(|(_, rest)| rest.trim_end_matches("\"]"))
                    .unwrap_or("")
                    .to_string();
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                    context,
                )
            })
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output, run with
    /// only `no-space-in-emphasis` enabled: its line, its `errorRange` column and length, and its
    /// `errorContext`.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so the two cases whose gaps sit after
    /// a multi-byte character are asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &str, &[Gap<'static>])] = &[
        ("valid_asterisks", "*a* and **b** and ***c***\n", &[]),
        ("valid_underscores", "_a_ and __b__ and ___c___\n", &[]),
        (
            "single_asterisk",
            "This has * invalid emphasis * with spaces inside.\n",
            &[(1, 11, 1, "* i"), (1, 28, 1, "s *")],
        ),
        (
            "double_asterisk",
            "This has ** invalid strong ** with spaces inside.\n",
            &[(1, 12, 1, "** i"), (1, 27, 1, "g **")],
        ),
        (
            "triple_asterisk",
            "This has *** invalid strong emphasis *** with spaces inside.\n",
            &[(1, 13, 1, "*** i"), (1, 37, 1, "s ***")],
        ),
        (
            "single_underscore",
            "This has _ invalid emphasis _ with spaces inside.\n",
            &[(1, 11, 1, "_ i"), (1, 28, 1, "s _")],
        ),
        (
            "double_underscore",
            "This has __ invalid strong __ with spaces inside.\n",
            &[(1, 12, 1, "__ i"), (1, 27, 1, "g __")],
        ),
        (
            "triple_underscore",
            "This has ___ invalid strong emphasis ___ with spaces inside.\n",
            &[(1, 13, 1, "___ i"), (1, 37, 1, "s ___")],
        ),
        ("mismatched_markers", "a * b ** c\n", &[]),
        ("mismatched_lengths", "a ** b *** c\n", &[]),
        ("opening_only", "a * invalid* b\n", &[(1, 4, 1, "* i")]),
        ("closing_only", "a *invalid * b\n", &[(1, 11, 1, "d *")]),
        (
            "three_markers",
            "a * b * c * d\n",
            &[(1, 4, 1, "* b"), (1, 6, 1, "b *")],
        ),
        (
            "four_markers",
            "a * b * c * d * e\n",
            &[
                (1, 4, 1, "* b"),
                (1, 6, 1, "b *"),
                (1, 12, 1, "* d"),
                (1, 14, 1, "d *"),
            ],
        ),
        (
            "wide_gaps",
            "a *   b   * c\n",
            &[(1, 4, 3, "*   b"), (1, 8, 3, "b   *")],
        ),
        ("marker_at_line_end", "a *\n", &[]),
        ("marker_at_line_start", "* a\n", &[]),
        (
            "nothing_between",
            "a *  * b\n",
            &[(1, 4, 2, "*  *"), (1, 4, 2, "*  *")],
        ),
        ("escaped", "a \\* not emph \\* b\n", &[]),
        (
            "escaped_backslash",
            "a \\\\* real * b\n",
            &[(1, 6, 1, "* r"), (1, 11, 1, "l *")],
        ),
        ("run_of_four", "a **** b **** c\n", &[]),
        ("run_of_five", "a ***** b ***** c\n", &[]),
        (
            "run_of_four_then_one",
            "a **** b * c * d\n",
            &[(1, 11, 1, "* c"), (1, 13, 1, "c *")],
        ),
        (
            "intraword_underscore",
            "foo_bar_baz and _ real _ here\n",
            &[(1, 18, 1, "_ r"), (1, 23, 1, "l _")],
        ),
        (
            "intraword_asterisk",
            "a*b*c and * real * here\n",
            &[(1, 12, 1, "* r"), (1, 17, 1, "l *")],
        ),
        ("across_lines", "a *\nb * c\n", &[(2, 2, 1, "b *")]),
        (
            "across_lines_both",
            "a * b\nc * d\n",
            &[(1, 4, 1, "* b"), (2, 2, 1, "c *")],
        ),
        ("inside_emphasis", "*a * b*\n", &[]),
        ("inside_strong", "**a ** b**\n", &[]),
        (
            "matched_then_bare",
            "x _y_ and _ z _ w\n",
            &[(1, 12, 1, "_ z"), (1, 14, 1, "z _")],
        ),
        ("code_span", "Regular `* invalid * code` here.\n", &[]),
        (
            "code_span_between",
            "a * `x` * b\n",
            &[(1, 4, 1, "* `"), (1, 8, 1, "` *")],
        ),
        ("indented_code", "text\n\n    * a *\n", &[]),
        ("fenced_code", "```\n* a *\n```\n", &[]),
        ("fenced_code_then", "```\n* a *\n```\n\n* b *\n", &[]),
        (
            "html_comment",
            "<!-- * a * -->\ntext * y *\n",
            &[(2, 7, 1, "* y"), (2, 9, 1, "y *")],
        ),
        (
            "html_inline",
            "a <b>* c *</b> d\n",
            &[(1, 7, 1, "* c"), (1, 9, 1, "c *")],
        ),
        ("math_block", "$$\neCPM = 0.03 * 0.3 * 1000 = 9\n$$\n", &[]),
        ("inline_math", "cost $a * b * c$ here\n", &[]),
        ("link_label", "[a * b * c](http://x)\n", &[]),
        (
            "link_destination",
            "[x](http://a * b * c)\n",
            &[(1, 15, 1, "* b"), (1, 17, 1, "b *")],
        ),
        (
            "bare_link",
            "<http://a * b * c>\n",
            &[(1, 12, 1, "* b"), (1, 14, 1, "b *")],
        ),
        (
            "reference_definition",
            "[a]: http://x * y * z\n",
            &[(1, 16, 1, "* y"), (1, 18, 1, "y *")],
        ),
        (
            "heading",
            "# * a *\n",
            &[(1, 4, 1, "* a"), (1, 6, 1, "a *")],
        ),
        (
            "heading_closed",
            "## * a * ##\n",
            &[(1, 5, 1, "* a"), (1, 7, 1, "a *")],
        ),
        ("setext", "* a *\n===\n", &[]),
        ("blockquote", "> * a *\n", &[]),
        ("list_item", "- * a *\n", &[]),
        (
            "table_cell",
            "| a | b |\n|---|---|\n| * x * | y |\n",
            &[(3, 4, 1, "* x"), (3, 6, 1, "x *")],
        ),
        (
            "table_delimiter_gap",
            "| * a * |\n|---|---|\n",
            &[(1, 4, 1, "* a"), (1, 6, 1, "a *")],
        ),
        (
            "adjacent_emphasis",
            "*a* * b * *c*\n",
            &[(1, 6, 1, "* b"), (1, 8, 1, "b *")],
        ),
        (
            "tabs_between",
            "a *\tb\t* c\n",
            &[(1, 4, 1, "*\tb"), (1, 6, 1, "b\t*")],
        ),
        ("trailing_marker_only", "text *\n", &[]),
        (
            "strong_pair_shape",
            "x ** **`code` ****,**** y\n",
            &[(1, 5, 1, "** *"), (1, 5, 1, "* **")],
        ),
        (
            "no_trailing_newline",
            "a * b *",
            &[(1, 4, 1, "* b"), (1, 6, 1, "b *")],
        ),
        (
            "crlf",
            "a * b *\r\nc * d\r\n",
            &[(1, 4, 1, "* b"), (1, 6, 1, "b *")],
        ),
        ("front_matter", "---\ntitle: * a *\n---\n\n* b *\n", &[]),
        ("nested_quote_list", "> - * a *\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(name, source, expected) in CASES {
            let found = gaps(source);
            let reported: Vec<Gap<'_>> = found
                .iter()
                .map(|(line, column, width, context)| (*line, *column, *width, context.as_str()))
                .collect();
            assert_eq!(expected, reported.as_slice(), "case `{name}`");
        }
    }

    /// The two cases where a gap follows a multi-byte character. markdownlint counts UTF-16 units,
    /// so its columns and widths are smaller than the byte-based ones quickmark reports; only the
    /// number of gaps agrees. This is the byte-column convention every rule shares, not an MD037
    /// difference.
    #[test]
    fn positions_count_bytes() {
        assert_eq!(2, gaps("你 * 好 * 世\n").len());
        assert_eq!(2, gaps("a *\u{a0}b\u{a0}* c\n").len());
    }

    #[test]
    fn a_marker_with_no_partner_in_its_scope_is_quiet() {
        // The inner marker sits inside the emphasis, so it has nothing to pair with.
        assert_eq!(0, gaps("*a * b*\n").len());
        // One marker per line, in separate scopes.
        assert_eq!(0, gaps("* a\n\n* b\n").len());
    }

    #[test]
    fn a_document_without_markers_is_quiet() {
        assert_eq!(0, gaps("plain *text* with _matched_ emphasis\n").len());
    }
}
