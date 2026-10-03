use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

/// markdownlint's `reversedLinkRe`. Its `(?!\()` has no counterpart here — the `regex` crate has no
/// lookahead — so [`MD011Linter::analyze`] checks the byte after the match instead.
static REVERSED_LINK_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[^\\])\(([^()]+)\)\[([^\]^][^\]]*)\]").unwrap());

/// MD011 Reversed Link Syntax Rule Linter
///
/// Line-based, like markdownlint's: the pattern is what decides, and the tree only says which lines
/// and spans to leave alone.
pub(crate) struct MD011Linter {
    context: Rc<Context>,
    /// The 0-based rows markdownlint skips whole, because a code or math block covers them.
    ignored_rows: HashSet<usize>,
    /// The byte spans markdownlint skips a match in, because a code span or a math span covers it.
    /// One of these may cross lines, which is why they are spans and not rows.
    ignored_spans: Vec<(usize, usize)>,
}

impl MD011Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            ignored_rows: HashSet::new(),
            ignored_spans: Vec::new(),
        }
    }

    /// The rows a block covers. Its end swallows the trailing newline, so the last byte it claims
    /// belongs to the row before the one `end_position` names.
    fn cover(&mut self, node: Node) {
        let from = node.start_position().row;
        let to = self.context.point_at(node.end_byte().saturating_sub(1)).row;
        self.ignored_rows.extend(from..=to);
    }

    /// The spans of the inline runs markdownlint ignores, found by walking the subtree `feed` hands
    /// over — inline kinds are filtered out of dispatch, so it never descends into one itself.
    fn collect_spans(&mut self, node: Node, out: &mut Vec<(usize, usize)>) {
        match node.kind() {
            "code_span" | "math" => out.push((node.start_byte(), node.end_byte())),
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.collect_spans(child, out);
                }
            }
        }
    }

    fn analyze(&self) -> Vec<RuleViolation> {
        let lines = self.context.lines.borrow();
        let mut violations = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            if self.ignored_rows.contains(&row) {
                continue;
            }
            let base = self.context.line_start_byte(row);
            for caps in REVERSED_LINK_REGEX.captures_iter(line) {
                let full_match = caps.get(0).unwrap();
                let pre_char = caps.get(1).unwrap();
                // markdownlint's `(?!\()`: a `(` straight after the match makes it something else.
                if line.as_bytes().get(full_match.end()) == Some(&b'(') {
                    continue;
                }
                let link_text = caps.get(2).unwrap().as_str();
                let destination = caps.get(3).unwrap().as_str();
                if link_text.ends_with('\\') || destination.ends_with('\\') {
                    continue;
                }
                let (from, to) = (pre_char.end(), full_match.end());
                if self.overlaps_ignored(base + from, base + to) {
                    continue;
                }
                violations.push(self.violation(row, from, to - from, &line[from..to]));
            }
        }
        violations
    }

    /// markdownlint's `hasOverlap` on ranges that may sit on different lines, which is what lets a
    /// code span covering a line break hide a match on the line after it.
    fn overlaps_ignored(&self, from: usize, to: usize) -> bool {
        self.ignored_spans
            .iter()
            .any(|&(start, end)| from < end && start < to)
    }

    fn violation(&self, row: usize, column: usize, width: usize, text: &str) -> RuleViolation {
        let start_byte = self.context.line_start_byte(row) + column;
        RuleViolation::new(
            &MD011,
            format!("{} [{text}]", MD011.description),
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
        )
    }
}

impl RuleLinter for MD011Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            // markdownlint also skips a `mathFlow`, which is what a `$$` block is here.
            "fenced_code_block" | "indented_code_block" | "math_block" => self.cover(*node),
            "inline" => {
                let mut spans = Vec::new();
                self.collect_spans(*node, &mut spans);
                self.ignored_spans.extend(spans);
            }
            _ => {}
        }
    }

    /// The scan waits for `finalize` because a `$$` block or a code span can start after the line
    /// it hides.
    fn finalize(&mut self) -> Vec<RuleViolation> {
        self.analyze()
    }
}

pub const MD011: Rule = Rule {
    id: "MD011",
    alias: "no-reversed-links",
    tags: &["links"],
    description: "Reversed link syntax",
    rule_type: RuleType::Line,
    required_nodes: &[
        "indented_code_block",
        "fenced_code_block",
        "math_block",
        "inline",
    ],
    new_linter: |context| Box::new(MD011Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// A report: the 1-based line and column of the reversed link, and the text markdownlint puts in
    /// the message.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);
    type Case = (&'static str, &'static str, &'static [Report]);

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-reversed-links", RuleSeverity::Error)])
    }

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, text)| (line, column, text.to_string()))
            .collect()
    }

    fn reports(input: &str) -> Vec<Found> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
        let mut found: Vec<Found> = linter
            .analyze()
            .iter()
            .map(|violation| {
                let text = violation
                    .message()
                    .split_once('[')
                    .and_then(|(_, rest)| rest.strip_suffix(']'))
                    .unwrap_or_default();
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    text.to_string(),
                )
            })
            .collect();
        found.sort();
        found
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "a reversed link on its own",
                "(text)[url]\n",
                &[(1, 1, "(text)[url]")],
            ),
            ("the correct syntax", "[text](url)\n", &[]),
            ("inside a code span", "`(text)[url]`\n", &[]),
            (
                "inside a code span of two backticks",
                "``(text)[url]``\n",
                &[],
            ),
            ("inside a fenced code block", "```\n(text)[url]\n```\n", &[]),
            ("inside an indented code block", "    (text)[url]\n", &[]),
            (
                "with an escaped opening parenthesis",
                "\\(text)[url]\n",
                &[],
            ),
            (
                "with a link text ending in a backslash",
                "(text\\)[url]\n",
                &[],
            ),
            ("followed by an opening parenthesis", "(text)[url](\n", &[]),
            (
                "between other text",
                "a(text)[url]b\n",
                &[(1, 2, "(text)[url]")],
            ),
            ("inside inline math", "$(x)[y]$ math\n", &[]),
            ("inside a math block", "$$\n(x)[y]\n$$\n", &[]),
            (
                "twice on one line",
                "(a)[b] and (c)[d]\n",
                &[(1, 1, "(a)[b]"), (1, 12, "(c)[d]")],
            ),
            ("in a heading", "# (text)[url]\n", &[(1, 3, "(text)[url]")]),
            (
                "in a block quote",
                "> (text)[url]\n",
                &[(1, 3, "(text)[url]")],
            ),
            (
                "in a list item",
                "- (text)[url]\n",
                &[(1, 3, "(text)[url]")],
            ),
            (
                "in a table cell",
                "| a |\n| - |\n| (text)[url] |\n",
                &[(3, 3, "(text)[url]")],
            ),
            (
                "on the line after a fenced code block",
                "```\ncode\n```\n(text)[url]\n",
                &[(4, 1, "(text)[url]")],
            ),
            ("with an empty link text", "()[x]\n", &[]),
            (
                "with brackets for a link text",
                "([])[x]\n",
                &[(1, 1, "([])[x]")],
            ),
            (
                "with brackets inside the link text",
                "([a])[x]\n",
                &[(1, 1, "([a])[x]")],
            ),
            ("with no closing bracket", "(text)[\n", &[]),
            ("with an empty destination", "(text)[]\n", &[]),
            (
                "with a carriage return",
                "(text)[url]\r\n",
                &[(1, 1, "(text)[url]")],
            ),
            (
                "inside a code span that spans lines",
                "`a\n(text)[url]`\n",
                &[],
            ),
            (
                "below a blank line after a fenced code block",
                "```\ncode\n```\n\n(text)[url]\n",
                &[(5, 1, "(text)[url]")],
            ),
            (
                "inside an html block",
                "<div>\n(text)[url]\n</div>\n",
                &[(2, 1, "(text)[url]")],
            ),
            (
                "after a code span",
                "x `a` (text)[url]\n",
                &[(1, 7, "(text)[url]")],
            ),
            (
                "before an unmatched backtick",
                "(text)[url] `\n",
                &[(1, 1, "(text)[url]")],
            ),
            (
                "in the first of two paragraphs",
                "text (a)[b]\n\nmore\n",
                &[(1, 6, "(a)[b]")],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input), "{name}");
        }
    }

    #[test]
    fn a_report_covers_the_link() {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), "a(b)[c]\n");
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 1, 0, 7),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }
}
