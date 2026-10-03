use crate::ast::{Node, NodeRange, Point};
use std::rc::Rc;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{ellipsify, is_blank_line, Rule, RuleType};

pub(crate) struct MD032Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD032Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// A row the document does not have is blank, which is what keeps a list at either end of the
    /// file from reporting.
    fn is_blank(row: usize, lines: &[String]) -> bool {
        lines.get(row).is_none_or(|line| is_blank_line(line))
    }

    /// Whether a list is one markdownlint looks at. It descends into every token except lists and
    /// HTML flows, so a list inside a block quote is checked and a list inside a list is not — the
    /// outer list already accounts for the blank lines around the whole of it. An `html_block` is a
    /// leaf here, so the HTML-flow half of that needs no code.
    fn is_top_level(node: Node) -> bool {
        let mut current = node.parent();
        while let Some(parent) = current {
            if parent.kind() == "list" {
                return false;
            }
            current = parent.parent();
        }
        true
    }

    /// The row after the last one the list can claim.
    ///
    /// A list's range runs on over trailing blank rows to wherever the next block starts, so that
    /// row belongs to the next block rather than to the list. With no next block the range runs to
    /// the parent's end instead, which is one past the parent's last row — unless the document has
    /// no final newline, in which case it names that last row itself.
    fn claimed_end_row(&self, node: Node, lines: &[String]) -> usize {
        if let Some(next) = node.next_sibling() {
            return next.start_position().row;
        }
        let end = node.end_position();
        let last_row = lines.len().saturating_sub(1);
        if end.row > last_row || self.context.document_content.borrow().ends_with('\n') {
            end.row
        } else {
            end.row + 1
        }
    }

    /// The list's last row that carries content, skipping the trailing blank rows its range covers.
    fn last_content_row(&self, node: Node, lines: &[String]) -> usize {
        let first = node.start_position().row;
        let upper = self.claimed_end_row(node, lines).saturating_sub(1);
        (first..=upper.max(first))
            .rev()
            .find(|&row| !Self::is_blank(row, lines))
            .unwrap_or(first)
    }

    fn check(&mut self, node: Node) {
        if !Self::is_top_level(node) {
            return;
        }

        // Both rows are settled before anything is reported: reporting needs `&mut self`, and the
        // line table is borrowed off `self`.
        let (before, after) = {
            let lines = self.context.lines.borrow();
            let first = node.start_position().row;
            let last = self.last_content_row(node, &lines);
            // markdownlint reads the row above the list and the row below its last content row —
            // not below the range's end, which trailing blank rows push further on.
            let before = (first > 0 && !Self::is_blank(first - 1, &lines)).then_some(first);
            // A missing blank below is reported on the list's own last line, not on the line that
            // should have been blank.
            let after = (!Self::is_blank(last + 1, &lines)).then_some(last);
            (before, after)
        };

        for row in [before, after].into_iter().flatten() {
            self.report(row);
        }
    }

    fn report(&mut self, row: usize) {
        let lines = self.context.lines.borrow();
        let line = lines.get(row).map_or("", String::as_str);
        let width = line.len();
        // markdownlint quotes the list's own line at whichever end is missing its blank.
        let message = format!(
            "{} [Context: \"{}\"]",
            MD032.description,
            ellipsify(line.trim(), false, false)
        );
        self.violations.push(RuleViolation::new(
            &MD032,
            message,
            self.context.file_path.clone(),
            range_from_node_range(&NodeRange {
                start_byte: 0,
                end_byte: 0,
                start_point: Point { row, column: 0 },
                end_point: Point { row, column: width },
            }),
        ));
    }
}

impl RuleLinter for MD032Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "list" {
            self.check(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD032: Rule = Rule {
    id: "MD032",
    aliases: &["blanks-around-lists"],
    tags: &["blank_lines", "bullet", "ol", "ul"],
    description: "Lists should be surrounded by blank lines",
    rule_type: RuleType::Token,
    required_nodes: &["list"],
    new_linter: |context| Box::new(MD032Linter::new(context)),
};

#[cfg(test)]
mod test {
    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;
    use std::path::PathBuf;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("blanks-around-lists", RuleSeverity::Error)])
    }

    /// A case's name, its document, and the 1-based lines markdownlint reports on. A missing blank
    /// above is reported on the list's first line and one below on its last content line, so the line
    /// is what tells the two apart.
    type Case = (&'static str, &'static str, &'static [usize]);

    fn reports(source: &str) -> Vec<usize> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD032")
            .map(|violation| violation.location().range.start.line + 1)
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3. It names the list's own first
    /// line for a missing blank above, and the list's own last content line — not the line that
    /// should have been blank — for a missing one below.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            ("surrounded", "text\n\n- a\n- b\n\ntext\n", &[]),
            ("nothing above", "text\n- a\n- b\n\ntext\n", &[2]),
            // `text` continues the last item's paragraph lazily, so the list reaches the end.
            ("lazy continuation below", "text\n\n- a\n- b\ntext\n", &[]),
            ("neither blank", "text\n- a\n- b\ntext\n", &[2]),
            ("at the document start", "- a\n- b\n\ntext\n", &[]),
            ("at the document end", "text\n\n- a\n- b\n", &[]),
            ("the whole document", "- a\n- b\n", &[]),
            ("no final newline", "- a\n- b", &[]),
            ("no final newline, text above", "text\n- a\n- b", &[2]),
            ("thematic break below", "- a\n- b\n---\n", &[2]),
            ("heading below", "- a\n- b\n# H\n", &[2]),
            ("fence below", "- a\n- b\n```\ncode\n```\n", &[2]),
            ("blank then thematic break", "- a\n- b\n\n---\n", &[]),
            ("blank then heading", "- a\n- b\n\n# H\n", &[]),
            ("thematic break above", "---\n- a\n- b\n", &[2]),
            ("heading above", "# H\n- a\n- b\n", &[2]),
            ("fence above", "```\ncode\n```\n- a\n- b\n", &[4]),
            ("quoted, blank `>` around", "> text\n>\n> - a\n> - b\n>\n> text\n", &[]),
            ("quoted, nothing around", "> text\n> - a\n> - b\n> text\n", &[2]),
            ("quoted, blank `>` above", "> text\n>\n> - a\n> - b\n", &[]),
            ("quoted, blank `>` below", "> - a\n> - b\n>\n> text\n", &[]),
            ("doubly quoted", ">> - a\n>> - b\n", &[]),
            ("nested items", "- a\n  - nested\n  - nested\n- b\n\ntext\n", &[]),
            ("loose", "text\n\n- a\n\n- b\n\ntext\n", &[]),
            ("loose, lazy below", "text\n\n- a\n\n- b\ntext\n", &[]),
            ("loose, thematic break below", "- a\n\n- b\n---\n", &[3]),
            ("ordered", "text\n\n1. a\n2. b\n\ntext\n", &[]),
            ("ordered, neither blank", "text\n1. a\n2. b\n---\n", &[2, 3]),
            // `+ a` and `- b` are two lists, so each reports against the other.
            ("mixed markers", "text\n\n+ a\n- b\n\ntext\n", &[3, 4]),
            ("comment below", "- a\n- b\n<!-- c -->\ntext\n", &[]),
            ("comment above", "text\n<!-- c -->\n- a\n- b\n", &[]),
            ("comment below then blank", "- a\n- b\n<!-- c -->\n\ntext\n", &[]),
            ("quote below", "- a\n- b\n\n> quote\n", &[]),
            ("quote below, no blank", "- a\n- b\n> quote\n", &[2]),
            ("quote above", "> quote\n- a\n- b\n", &[2]),
            ("table below", "- a\n- b\n\n| x |\n| - |\n", &[]),
            ("table below, no blank", "- a\n- b\n| x |\n| - |\n", &[]),
            ("indented code below", "- a\n- b\n\n    indented\n", &[]),
            ("item ending in indented code", "- a\n\n      code\n\n- b\n\ntext\n", &[]),
            ("item ending in a closed fence", "- a\n\n  ```\n  code\n  ```\n\ntext\n", &[]),
            // The closing fence is the list's last content line, so the report lands there.
            (
                "item ending in a fence, text below",
                "- a\n\n  ```\n  code\n  ```\ntext\n",
                &[5],
            ),
            ("two lists, blank between", "- a\n- b\n\n- c\n- d\n", &[]),
            ("one long list", "- a\n- b\n- c\n- d\n", &[]),
            ("two-line paragraph above", "para one\npara two\n- a\n- b\n", &[3]),
            ("two-line lazy continuation", "- a\n- b\npara one\npara two\n", &[]),
            ("two-line paragraph below", "- a\n- b\n\npara one\npara two\n", &[]),
            ("setext underline below", "- a\n- b\n=====\n", &[]),
            ("setext heading below", "- a\n- b\n\nSetext\n======\n", &[]),
            ("loose item, blank below", "- item\n\n  more\n\ntext\n", &[]),
            ("loose item, lazy below", "- item\n\n  more\ntext\n", &[]),
            ("several blanks below", "- a\n- b\n\n\n\ntext\n", &[]),
            ("several blanks above", "text\n\n\n\n- a\n- b\n", &[]),
            ("indented list", "   - a\n   - b\n\ntext\n", &[]),
            ("indented list, text above", "text\n   - a\n   - b\n", &[2]),
            ("html block below", "- a\n- b\n<div>\nx\n</div>\n", &[2]),
            // An unclosed type-6 HTML block runs to the end of the document and swallows the list,
            // so there is no list to report on at all.
            ("html block above", "<div>\nx\n</div>\n- a\n- b\n", &[]),
            ("crlf", "- a\r\n- b\r\n\r\ntext\r\n", &[]),
            ("empty comment below", "- a\n- b\n\n<!-- -->\n", &[]),
            ("bare `>` below a plain list", "- a\n- b\n>\n> text\n", &[]),
            ("empty item below", "- a\n- b\n\n-\n", &[]),
            ("lazy continuation in an ordered list", "1. List item\n   More item 1\n2. List item\nMore item 2\n\ntail\n", &[]),
            ("nested list then a quote", "- a\n  - nested\n\n> quote\n\ntail\n", &[]),
            // The two shapes the vault comparison turned up: a quoted list whose range ran on into
            // the paragraph after the closing `>`.
            (
                "quoted ordered list with continuations",
                "> **Two corrections.** worth\n> recording because.\n>\n> 1. *\"Whichever replica completes\n>    other five.\"* **Wrong.** columns\n>    and more\n> 2. second item\n>\n> What survives below\n",
                &[],
            ),
            (
                "quoted list after a quoted fence",
                "> ```\n> code\n> ```\n>\n> A `Foo` will need to:\n>\n> - store some integer\n> - Enough space\n>\n> => though Empty\n",
                &[],
            ),
            ("two comment lines below", "- a\n- b\n\n<!-- one -->\n<!-- two -->\ntext\n", &[]),
            ("unterminated comment below", "- a\n- b\n<!-- unterminated\ntext\n", &[]),
            ("unmatched close below", "- a\n- b\n--> rest\n", &[]),
            ("unmatched close above", "text -->\n- a\n- b\n", &[]),
            ("math block below", "- a\n- b\n\n$$\nx\n$$\n", &[]),
            ("quoted thematic break below", "> - a\n> - b\n> ---\n", &[2]),
            ("whitespace-only line between items", "- a\n- b\n   \n- c\n", &[]),
            ("two lists, nothing between", "+ a\n- b\n", &[1, 2]),
            ("tab-only line below", "- a\n- b\n\t\ntext\n", &[]),
            ("quoted list inside a quote below", "- a\n- b\n\n> x\n>\n> - c\n> - d\n", &[]),
            ("loose list, lazy below", "text\n\n- a\n- b\n\n- c\n- d\ntext\n", &[]),
            ("loose list to the end", "- a\n\n- b\n\n- c\n", &[]),
            (
                "html block between two lists",
                "- a\n- b\n<div>\nx\n</div>\n\n- c\n- d\n",
                &[2],
            ),
            ("blank first line", "   \n- a\n- b\n", &[]),
            ("trailing blanks only", "- a\n- b\n\n\n", &[]),
        ];

        let failures: Vec<String> = cases
            .iter()
            .filter(|&&(name, source, expected)| {
                let actual = reports(source);
                let expected: Vec<usize> = expected.to_vec();
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

    /// markdownlint quotes the list's own line trimmed — the first one for a missing blank above and
    /// the last content one for a missing blank below.
    #[test]
    fn a_report_quotes_the_list_line() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            test_config(),
            "text\n- a\n- b\ntext\n",
        );
        let messages: Vec<String> = linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD032")
            .map(|violation| violation.message().to_string())
            .collect();
        assert_eq!(
            vec!["Lists should be surrounded by blank lines [Context: \"- a\"]"],
            messages
        );
    }

    /// `$$` on the line after a list opens a math *flow* in micromark, which interrupts the list. In
    /// comrak it is a lazy continuation of the last item's paragraph, so the list absorbs it and has
    /// nothing after it to be blank about. markdownlint reports line 2 here. Closing the gap means
    /// splitting a comrak paragraph at a `$$` line — see `ast::synth::math_regions`.
    #[test]
    fn a_math_flow_interrupting_a_list_is_a_known_difference() {
        assert_eq!(reports("- a\n- b\n$$\nx\n$$\n"), vec![]);
    }

    #[test]
    fn a_report_covers_its_whole_line() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            test_config(),
            "> text\n> - a\n> - b\n",
        );
        let range = linter
            .analyze()
            .iter()
            .find(|violation| violation.rule().id == "MD032")
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
        assert_eq!(range, (1, 0, 1, 5));
    }
}
