use crate::ast::{Node, NodeRange, Point};
use std::rc::Rc;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{Rule, RuleType};

/// micromark's tab stop.
const TAB_SIZE: usize = 4;

pub(crate) struct MD023Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

/// One container a heading sits inside, and what its prefix takes from the line.
enum Container {
    /// A block quote's `>` and the one space or tab after it.
    Quote,
    /// A list item. On the item's own first line the prefix is the marker and whatever whitespace
    /// follows it; on a later line it is indentation up to the item's content column.
    Item {
        content_column: usize,
        first_line: bool,
    },
}

impl MD023Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// The containers enclosing `heading`, outermost first.
    fn containers(&self, heading: Node, row: usize) -> Vec<Container> {
        let mut chain = Vec::new();
        let mut current = heading.parent();
        while let Some(node) = current {
            match node.kind() {
                "block_quote" => chain.push(Container::Quote),
                "list_item" => chain.push(Container::Item {
                    content_column: item_content_column(node),
                    first_line: row == node.start_position().row,
                }),
                _ => {}
            }
            current = node.parent();
        }
        chain.reverse();
        chain
    }

    fn check(&mut self, node: Node) {
        let row = node.start_position().row;
        let indent = {
            let lines = self.context.lines.borrow();
            let Some(line) = lines.get(row) else {
                return;
            };
            leading_indent(line, &self.containers(node, row))
        };
        let Some((from, to)) = indent else { return };
        self.violations.push(RuleViolation::new(
            &MD023,
            MD023.description.to_string(),
            self.context.file_path.clone(),
            range_from_node_range(&NodeRange {
                start_byte: 0,
                end_byte: 0,
                start_point: Point { row, column: from },
                end_point: Point { row, column: to },
            }),
        ));
    }
}

/// The column a list item's content starts at, which is where its marker ends. comrak counts this
/// in visual columns with tabs already expanded, which is what the continuation-line arithmetic
/// below needs.
fn item_content_column(item: Node) -> usize {
    let mut cursor = item.walk();
    item.children(&mut cursor)
        .find(|child| child.kind().starts_with("list_marker_"))
        .map_or(0, |marker| marker.end_position().column)
}

/// The byte a list item's prefix reaches on its own first line: past up to three spaces of
/// indentation, the marker, and up to four columns of whitespace after it.
///
/// Measured off the line rather than taken from [`item_content_column`], because that counts a tab
/// as the columns it covers while a byte offset cannot: for `-\t# H` the content column is four but
/// the heading starts at byte two.
fn marker_prefix_end(bytes: &[u8], mut column: usize) -> usize {
    let mut spaces = 0;
    while spaces < 3 && bytes.get(column) == Some(&b' ') {
        column += 1;
        spaces += 1;
    }
    match bytes.get(column) {
        Some(b'-' | b'*' | b'+') => column += 1,
        Some(b'0'..=b'9') => {
            let mut digits = 0;
            while digits < 9 && matches!(bytes.get(column), Some(b'0'..=b'9')) {
                column += 1;
                digits += 1;
            }
            if !matches!(bytes.get(column), Some(b'.') | Some(b')')) {
                return column;
            }
            column += 1;
        }
        _ => return column,
    }
    let mut after = 0;
    while after < 4 && matches!(bytes.get(column), Some(b' ') | Some(b'\t')) {
        column += 1;
        after += 1;
    }
    column
}

/// The run of whitespace between a line's container prefixes and its heading, as the column it
/// starts at and the column the heading starts at — or `None` when there is no such run, which is
/// the only case markdownlint leaves alone.
///
/// It reports on micromark's `linePrefix`, the whitespace a container's own prefix does not
/// account for. A list item's indentation is part of its prefix, so a heading indented to the
/// item's content column is flush left as far as this rule is concerned; only what is indented
/// *past* that counts.
///
/// A tab is wider than the space it stands in for and micromark keeps the difference, so `>\t# H`
/// has a zero-width `linePrefix` between the quote and the heading. Columns are therefore tracked
/// the way micromark counts them — from one, in tab stops of four — and any overshoot counts as
/// indentation even when no character is left to show for it.
fn leading_indent(line: &str, containers: &[Container]) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut column = 0usize;
    let mut visual = 1usize;
    let mut overshoot = 0usize;

    for container in containers {
        match container {
            Container::Quote => {
                if bytes.get(column) != Some(&b'>') {
                    continue;
                }
                visual = step(visual, bytes[column]);
                column += 1;
                if matches!(bytes.get(column), Some(b' ') | Some(b'\t')) {
                    let before = visual;
                    visual = step(visual, bytes[column]);
                    column += 1;
                    overshoot += visual - (before + 1);
                }
            }
            Container::Item {
                content_column,
                first_line,
            } => {
                if *first_line {
                    // The marker and the whitespace after it are the item's own prefix, and a tab
                    // among them is absorbed whole.
                    let limit = marker_prefix_end(bytes, column);
                    while column < limit {
                        visual = step(visual, bytes[column]);
                        column += 1;
                    }
                } else {
                    // Indentation stops as soon as it reaches the content column, and a tab that
                    // jumps past it leaves the difference behind.
                    let target = content_column + 1;
                    while visual < target && matches!(bytes.get(column), Some(b' ') | Some(b'\t')) {
                        visual = step(visual, bytes[column]);
                        column += 1;
                    }
                    overshoot += visual.saturating_sub(target);
                }
            }
        }
    }

    let heading = (column..bytes.len()).find(|&at| !matches!(bytes[at], b' ' | b'\t'))?;
    (heading > column || overshoot > 0).then_some((column, heading))
}

/// The column after `byte`, counting from one and stopping a tab at the next multiple of four.
fn step(column: usize, byte: u8) -> usize {
    if byte == b'\t' {
        column + (TAB_SIZE - (column - 1) % TAB_SIZE)
    } else {
        column + 1
    }
}

impl RuleLinter for MD023Linter {
    fn feed(&mut self, node: &Node) {
        if matches!(node.kind(), "atx_heading" | "setext_heading") {
            self.check(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD023: Rule = Rule {
    id: "MD023",
    alias: "heading-start-left",
    tags: &["headings", "spaces"],
    description: "Headings must start at the beginning of the line",
    rule_type: RuleType::Token,
    required_nodes: &["atx_heading", "setext_heading"],
    new_linter: |context| Box::new(MD023Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("heading-start-left", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ])
    }

    /// The 1-based lines MD023 reports on.
    fn lines(source: &str) -> Vec<usize> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD023")
            .map(|violation| violation.location().range.start.line + 1)
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[(&str, &str, &[usize])] = &[
            ("two spaces", "  ## H\n", &[1]),
            ("one space", " ## H\n", &[1]),
            ("flush left", "### H\n", &[]),
            ("four spaces is code", "    ## H\n", &[]),
            ("in a block quote", "> ## H\n", &[]),
            ("extra indent in a block quote", ">   ## H\n", &[1]),
            ("two block quotes", "> > ## H\n", &[]),
            ("indented block quote", "  > ## H\n", &[1]),
            ("bare marker, no space", "> ## H\n", &[]),
            ("one extra space in a quote", ">  ## H\n", &[1]),
            // A tab stands in for the quote's one space and overshoots the column it should reach,
            // which micromark records as a zero-width prefix.
            ("tab after the marker", "> \t## H\n", &[1]),
            ("tab instead of the space", ">\t## H\n", &[1]),
            ("tab then a nested quote", ">\t> ## H\n", &[1]),
            ("tab is the whole indent", "\t## H\n", &[]),
            ("space then tab is code", "  \t## H\n", &[]),
            ("setext text indented", "  H\n  ===\n", &[1]),
            ("setext underline indented", "H\n  ===\n", &[]),
            ("setext text only indented", "  H\n===\n", &[1]),
            ("setext both indented", "  Setext\n  --------\n", &[1]),
            ("setext in a document", "Some text\n\nSetext heading\n ==============\n\nMore\n", &[]),
            ("setext text in a document", "Some text\n\n Setext heading\n ====\n\nMore\n", &[3]),
            ("setext text only", "Some text\n\n Indented setext heading\n====\n\nMore\n", &[3]),
            ("atx in a document", "Some text\n\n # Indented heading\n\nMore text\n", &[3]),
            ("three spaces in a document", "Some text\n\n   # Heading\n\nMore text\n", &[3]),
            ("several headings", " # One\n\n ## Two\n\n### Three\n\n   #### Four\n", &[1, 3, 7]),
            ("trailing spaces", "  # H  \n", &[1]),
            ("after a thematic break", "---\n\n  ## H\n", &[3]),
            ("empty marker", "#\n", &[]),
            ("indented empty marker", " #\n", &[1]),
            ("empty document", "", &[]),
            ("code block", "```\n# code\n   # code\n```\n", &[]),
            ("inline code", "Text with `# inline code` and more text\n", &[]),
            ("quoted heading", "> # Heading\n\n> More content\n", &[]),
            // A list item's own indentation is part of its prefix, so a heading at the item's
            // content column is flush left as far as this rule is concerned.
            ("item on the marker's line", "- # H\n", &[]),
            ("ordered item on the marker's line", "1. # x\n", &[]),
            ("second item on its marker's line", "- a\n- # H\n", &[]),
            ("nested item's marker line", "- item\n  - # x\n", &[]),
            ("heading under a quote in a list", "> - # x\n", &[]),
            ("continuation at the content column", "- a\n  ## H\n", &[]),
            ("continuation after a blank", "- a\n\n  ## H\n", &[]),
            ("ordered continuation at the column", "1. a\n\n   # H\n", &[]),
            ("ordered continuation past it", "1. a\n\n    ## H\n", &[3]),
            ("wide marker at the column", "-   a\n\n    ## H\n", &[]),
            ("wide marker past it", "-   a\n\n     ## H\n", &[3]),
            ("two space marker at the column", "-  a\n\n   ## H\n", &[]),
            ("digits marker at the column", "10. a\n\n    ## H\n", &[]),
            ("digits marker past it", "10. a\n\n     ## H\n", &[3]),
            ("five spaces is code", "-     ## H\n", &[]),
            ("indented past the marker", "- a\n     ## H\n", &[2]),
            ("loose, indented past the marker", "- a\n\n   ## H\n", &[3]),
            ("loose, tab past the marker", "- a\n\n  \t## H\n", &[3]),
            ("tab as the item's whitespace", "-\t## H\n", &[]),
            ("tab as an ordered item's whitespace", "1.\t# H\n", &[]),
            ("setext at the content column", "- a\n\n  H\n  ===\n", &[]),
            ("quoted list, continuation at column", "> - a\n>\n>   ## H\n", &[]),
            ("quoted list, continuation past it", "> - a\n>      ## H\n", &[2]),
            ("quote at the content column", "- a\n\n  > ## H\n", &[]),
            ("quote past the content column", "- a\n\n  >   ## H\n", &[3]),
            ("quote with a tab after it", "- a\n\n  >\t## H\n", &[]),
            ("quoted continuation past it", "> a\n>\n>   ## H\n", &[3]),
            // The two shapes the vault comparison turned up.
            (
                "cjk list continuation",
                "- \u{91ca}\u{653e}\u{9884}\u{8bfb}\u{53d6}\u{670d}\u{52a1}\n\n  `releaseBlock(...)`\n\n  ## BlockPrefetchService\n\n  - `prefetchBlock`\n",
                &[],
            ),
            (
                "hash in a nested item's text",
                "- reads input data row by row\n- Segment can be published\n  - # of segment exceeds `X`\n  - # of rws added to `Y`\n",
                &[],
            ),
        ];

        let failures: Vec<String> = cases
            .iter()
            .filter(|&&(name, source, expected)| {
                let actual = lines(source);
                if actual == expected {
                    return false;
                }
                println!("{name}: expected {expected:?}, got {actual:?}");
                true
            })
            .map(|&(name, _, _)| name.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} cases disagree with markdownlint: {failures:?}",
            failures.len(),
            cases.len()
        );
    }

    /// markdownlint reports the `linePrefix` it found, and a tab that overshoots its container's
    /// prefix leaves one zero columns wide — which its own range check rejects, so the rule throws
    /// and markdownlint reports line 1 with "This rule threw an exception" instead of the heading's
    /// line. quickmark reports the heading.
    #[test]
    fn a_zero_width_prefix_is_a_known_difference() {
        assert_eq!(lines("- a\n\n\t## H\n"), vec![3]);
        assert_eq!(lines("- a\n\n\t> ## H\n"), vec![3]);
    }

    #[test]
    fn a_report_covers_the_indentation() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            test_config(),
            ">   ## H\n",
        );
        let range = linter
            .analyze()
            .iter()
            .find(|violation| violation.rule().id == "MD023")
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            })
            .expect("one violation");
        // markdownlint's range is the `linePrefix` itself, which starts after the quote's own `> `.
        assert_eq!(range, (0, 2, 0, 4));
    }
}
