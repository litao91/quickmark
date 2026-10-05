use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{marker_glyph, Context, Rule, RuleLinter, RuleType},
};

// MD030-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD030ListMarkerSpaceTable {
    pub ul_single: usize,
    pub ol_single: usize,
    pub ul_multi: usize,
    pub ol_multi: usize,
}

impl Default for MD030ListMarkerSpaceTable {
    fn default() -> Self {
        Self {
            ul_single: 1,
            ol_single: 1,
            ul_multi: 1,
            ol_multi: 1,
        }
    }
}

pub(crate) struct MD030Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD030Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }
}

impl RuleLinter for MD030Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "list" {
            self.check_list_marker_spacing(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

impl MD030Linter {
    fn check_list_marker_spacing(&mut self, list_node: &Node) {
        let list_items: Vec<Node> = {
            let mut cursor = list_node.walk();
            list_node
                .children(&mut cursor)
                .filter(|c| c.kind() == "list_item")
                .collect()
        };

        if list_items.is_empty() {
            return;
        }

        let is_ordered = self.is_ordered_list(&list_items[0]);
        let is_single_line = is_single_line_list(list_node, &list_items);

        let expected_spaces = self.get_expected_spaces(is_ordered, is_single_line);

        for list_item in &list_items {
            self.check_list_item_spacing(list_item, expected_spaces);
        }
    }

    fn is_ordered_list(&self, list_item_node: &Node) -> bool {
        let mut cursor = list_item_node.walk();
        let result = list_item_node
            .children(&mut cursor)
            .find(|c| c.kind().starts_with("list_marker"))
            .is_some_and(|marker_node| {
                let kind = marker_node.kind();
                kind == "list_marker_dot" || kind == "list_marker_parenthesis"
            });
        result
    }

    fn get_expected_spaces(&self, is_ordered: bool, is_single_line: bool) -> usize {
        let config = &self.context.config.linters.settings.list_marker_space;
        match (is_ordered, is_single_line) {
            (true, true) => config.ol_single,
            (true, false) => config.ol_multi,
            (false, true) => config.ul_single,
            (false, false) => config.ul_multi,
        }
    }

    fn check_list_item_spacing(&mut self, list_item: &Node, expected_spaces: usize) {
        let mut cursor = list_item.walk();
        let children: Vec<Node> = list_item.children(&mut cursor).collect();
        let Some(marker) = children
            .iter()
            .find(|child| child.kind().starts_with("list_marker"))
        else {
            return;
        };
        // An item with no content on the marker's own line — a bare `-` used as a spacer, or one
        // whose paragraph starts on the next line — gets no `listItemPrefixWhitespace` from
        // micromark, so there is nothing to judge.
        let Some(content) = children
            .iter()
            .find(|child| !child.kind().starts_with("list_marker"))
        else {
            return;
        };
        if content.start_position().row != marker.start_position().row {
            return;
        }

        let document_content = self.context.document_content.borrow();
        let Ok(text) = marker.utf8_text(document_content.as_bytes()) else {
            return;
        };
        let (start, glyph_len) = marker_glyph(*marker, text);
        let content_column = content.start_position().column;
        // micromark stops a prefix at four spaces after the marker; past that the item's content is
        // indented code starting one space in. comrak's tree already says so, which is what makes
        // the cap fall out here instead of needing a special case.
        let Some(actual_spaces) = content_column.checked_sub(start + glyph_len) else {
            return;
        };

        if actual_spaces == expected_spaces {
            return;
        }

        let message = format!(
            "{} [Expected: {}; Actual: {}]",
            MD030.description, expected_spaces, actual_spaces
        );
        // markdownlint underlines the whole prefix — marker and whitespace, but not the list's own
        // indentation — which is exactly `start..content_column`.
        let mut range = range_from_node_range(&marker.range());
        range.start.character = start;
        range.end.character = content_column;

        self.violations.push(RuleViolation::new(
            &MD030,
            message,
            self.context.file_path.clone(),
            range,
        ));
    }
}

/// Whether markdownlint judges `list_node` against `ul_single`/`ol_single` rather than the multi
/// settings: it compares the list's line count to its item count, so a blank line *between* two
/// one-line items makes the whole list multi-line, and so does an item that wraps or holds a second
/// paragraph. Judging each item on its own would read a loose list as single.
fn is_single_line_list(list_node: &Node, list_items: &[Node]) -> bool {
    last_content_row(*list_node) - list_node.start_position().row + 1 == list_items.len()
}

/// The last row of `node`'s subtree that holds content.
///
/// comrak folds trailing blank lines into a list's last item, so a list node's own end can sit rows
/// past its last content; micromark's `list.endLine` does not, and that is what markdownlint
/// compares the item count against.
fn last_content_row(node: Node) -> usize {
    let mut last = node;
    while let Some(child) = last.child(last.child_count().saturating_sub(1)) {
        last = child;
    }
    // A block's end position swallows its trailing newline, so one that ends on its own last row
    // reports column 0 of the next.
    let end = last.end_position();
    if end.column == 0 {
        end.row.saturating_sub(1)
    } else {
        end.row
    }
}

pub const MD030: Rule = Rule {
    id: "MD030",
    aliases: &["list-marker-space"],
    tags: &["ol", "ul", "whitespace"],
    description: "Spaces after list markers",
    rule_type: RuleType::Token,
    required_nodes: &["list"],
    new_linter: |context| Box::new(MD030Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, QuickmarkConfig, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::{test_config_with_rules, test_config_with_settings};

    use super::MD030ListMarkerSpaceTable;

    fn test_config() -> QuickmarkConfig {
        test_config_with_rules(vec![("list-marker-space", RuleSeverity::Error)])
    }

    /// One report's `(line, column, width, expected, actual)`, the first three 1-based. The column
    /// and width cover markdownlint's `listItemPrefix` — the marker glyph and the whitespace after
    /// it, but not the list's own indentation.
    type Report = (usize, usize, usize, usize, usize);

    /// A case's name, its document, and the reports markdownlint makes on it.
    type Case = (&'static str, &'static str, &'static [Report]);

    fn reports(config: QuickmarkConfig, input: &str) -> Vec<Report> {
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                // "Spaces after list markers [Expected: 1; Actual: 2]"
                let counts: Vec<usize> = violation
                    .message()
                    .split([':', ';'])
                    .filter_map(|part| part.trim().trim_end_matches(']').parse().ok())
                    .collect();
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                    counts[0],
                    counts[1],
                )
            })
            .collect()
    }

    fn check(cases: &[Case], config: QuickmarkConfig) {
        for &(name, source, expected) in cases {
            assert_eq!(
                expected,
                reports(config.clone(), source).as_slice(),
                "{name}"
            );
        }
    }

    #[test]
    fn test_default_unordered_list_single_space_no_violations() {
        let input = "* Item 1\n* Item 2\n* Item 3\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Default single space after unordered list marker should have no violations"
        );
    }

    #[test]
    fn test_default_ordered_list_single_space_no_violations() {
        let input = "1. Item 1\n2. Item 2\n3. Item 3\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Default single space after ordered list marker should have no violations"
        );
    }

    #[test]
    fn test_unordered_list_double_space_has_violations() {
        let input = "*  Item 1\n*  Item 2\n*  Item 3\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert!(
            !violations.is_empty(),
            "Double space after unordered list marker should have violations"
        );
    }

    #[test]
    fn test_ordered_list_double_space_has_violations() {
        let input = "1.  Item 1\n2.  Item 2\n3.  Item 3\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert!(
            !violations.is_empty(),
            "Double space after ordered list marker should have violations"
        );
    }

    #[test]
    fn test_mixed_list_types_independent() {
        let input = "* Item 1\n* Item 2\n\n1. Item 1\n2. Item 2\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Mixed list types with correct spacing should have no violations"
        );
    }

    #[test]
    fn test_single_line_vs_multi_line_lists() {
        // Single-line list - each item is on one line
        let input_single = "* Item 1\n* Item 2\n* Item 3\n";

        // Multi-line list - has content that spans multiple lines
        let input_multi = "*   Item 1\n\n    Second paragraph\n\n*   Item 2\n";

        let config = test_config();

        // Single-line list with default spacing (1 space)
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            config.clone(),
            input_single,
        );
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Single-line list with 1 space should be valid"
        );

        // Multi-line list with 3 spaces (will fail with default config expecting 1 space)
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input_multi);
        let violations = linter.analyze();
        assert!(
            !violations.is_empty(),
            "Multi-line list with 3 spaces should have violations when expecting 1"
        );
    }

    #[test]
    fn test_nested_lists_not_affected() {
        let input = "* Item 1\n  * Nested item 1\n  * Nested item 2\n* Item 2\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Nested lists with correct spacing should have no violations"
        );
    }

    #[test]
    fn test_three_spaces_after_marker_has_violations() {
        let input = "*   Item 1\n*   Item 2\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert!(
            !violations.is_empty(),
            "Three spaces after list marker should have violations with default config"
        );
    }

    #[test]
    fn test_plus_marker_type() {
        let input = "+ Item 1\n+ Item 2\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Plus marker with single space should have no violations"
        );
    }

    #[test]
    fn test_dash_marker_type() {
        let input = "- Item 1\n- Item 2\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Dash marker with single space should have no violations"
        );
    }

    // Every expectation below was measured against markdownlint-cli2 v0.23.3.

    #[test]
    fn test_empty_list_items_are_not_violations() {
        // A bare marker used as a spacer has no content, so there is no spacing to judge.
        for input in [
            "- item\n-\n- item2\n",
            "- item\n- \n- item2\n",
            "- item\n-   \n- item2\n",
            "- item\n1.\n- item2\n",
            "- item\n1.   \n- item2\n",
        ] {
            let config = test_config();
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            let violations = linter.analyze();
            assert_eq!(0, violations.len(), "unexpected violations for {input:?}");
        }
    }

    #[test]
    fn test_real_marker_spacing_still_reported() {
        for input in ["-  two spaces\n- item\n", "1.  two spaces\n1. item\n"] {
            let config = test_config();
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            let violations = linter.analyze();
            assert_eq!(1, violations.len(), "expected one violation for {input:?}");
        }
    }

    /// The four defaults are all 1, which hides the single/multi choice; this splits them far
    /// enough apart that the two readings disagree on every input below.
    fn split_spacing_config() -> QuickmarkConfig {
        test_config_with_settings(
            vec![("list-marker-space", RuleSeverity::Error)],
            LintersSettingsTable {
                list_marker_space: MD030ListMarkerSpaceTable {
                    ul_single: 1,
                    ol_single: 1,
                    ul_multi: 3,
                    ol_multi: 3,
                },
                ..Default::default()
            },
        )
    }

    /// Every expectation below is markdownlint v0.41.1's own `errorRange` and `errorDetail`, read
    /// back through its `lintSync` API under the default configuration.
    #[test]
    fn matches_markdownlints_error_range() {
        let cases: &[Case] = &[
            ("two spaces after a bullet", "-  text\n", &[(1, 1, 3, 1, 2)]),
            (
                "three spaces after a bullet",
                "-   text\n",
                &[(1, 1, 4, 1, 3)],
            ),
            (
                "four spaces after a bullet",
                "-    text\n",
                &[(1, 1, 5, 1, 4)],
            ),
            // Five or more spaces make the item's content indented code, so micromark stops the
            // prefix one space after the marker and there is nothing to report.
            ("five spaces after a bullet", "-     text\n", &[]),
            ("seven spaces after a bullet", "-       text\n", &[]),
            // The column counts from the marker glyph, so the list's own indent is outside the range.
            ("two spaces, indented one", " -  text\n", &[(1, 2, 3, 1, 2)]),
            (
                "two spaces, indented two",
                "  -  text\n",
                &[(1, 3, 3, 1, 2)],
            ),
            (
                "four spaces, indented three",
                "   -    text\n",
                &[(1, 4, 5, 1, 4)],
            ),
            ("two spaces after a dot", "1.  text\n", &[(1, 1, 4, 1, 2)]),
            (
                "three spaces after a dot",
                " 1.   text\n",
                &[(1, 2, 5, 1, 3)],
            ),
            // A parenthesis delimiter is a marker too.
            (
                "two spaces after a parenthesis",
                "1)  text\n",
                &[(1, 1, 4, 1, 2)],
            ),
            (
                "three spaces after a parenthesis, indented two",
                "  1)   text\n",
                &[(1, 3, 5, 1, 3)],
            ),
            ("one space is correct", "- text\n1. text\n1) text\n", &[]),
            ("a bare marker", "-\n", &[]),
            ("a marker with only trailing spaces", "-   \n", &[]),
            ("content on the next line", "-\n  text\n", &[]),
            (
                "nested, only the inner item is wrong",
                "- a\n  -  b\n",
                &[(2, 3, 3, 1, 2)],
            ),
            (
                "two wrong rows in one list",
                "-  a\n-   b\n",
                &[(1, 1, 3, 1, 2), (2, 1, 4, 1, 3)],
            ),
            (
                "every bullet glyph counts one",
                "*  a\n",
                &[(1, 1, 3, 1, 2)],
            ),
            ("a plus marker", "+   a\n", &[(1, 1, 4, 1, 3)]),
            // The glyph's width comes from the marker's digits, which the range has to cover.
            ("a five-digit marker", "12345.  a\n", &[(1, 1, 8, 1, 2)]),
            ("a five-digit marker with one space", "12345. a\n", &[]),
            (
                "a nested marker on the same row",
                "- -  a\n",
                &[(1, 3, 3, 1, 2)],
            ),
            ("a nested marker with one space", "- - a\n", &[]),
            (
                "a marker inside a block quote",
                "> -  a\n",
                &[(1, 3, 3, 1, 2)],
            ),
            ("a block quoted marker with one space", "> - a\n", &[]),
            // A task list marker is content, not a marker, so the spaces before it are what count.
            ("a task item", "- [x]  a\n", &[]),
            (
                "spaces before a task item's bracket",
                "-  [x] a\n",
                &[(1, 1, 3, 1, 2)],
            ),
            // comrak's marker span reaches past the content when a tab follows the marker, so the
            // glyph's width has to come from its kind rather than from trimming that span.
            ("a tab after a bullet is one column", "-\ta\n", &[]),
            ("a tab between spaces", "- \t a\n", &[(1, 1, 4, 1, 3)]),
            ("two bare markers", "-\n-\n", &[]),
            ("five spaces, then a bare marker", "-     a\n-\n", &[]),
            (
                "a marker with only trailing spaces, then a real one",
                "-  \n-  a\n",
                &[(2, 1, 3, 1, 2)],
            ),
        ];
        check(cases, test_config());
    }

    /// The four defaults are all 1, so the single/multi choice is invisible in a default
    /// configuration; this splits them to pin the distinction itself. markdownlint's test is
    /// `list.endLine - list.startLine + 1 === prefixes.length` — a list is single-line only when it
    /// spans exactly as many lines as it has items — so a blank line *between* two one-line items
    /// makes the whole list multi-line, while a trailing blank line that comrak folds into the last
    /// item does not.
    #[test]
    fn matches_markdownlints_single_line_test() {
        let cases: &[Case] = &[
            ("one item on one line", "-  a\n", &[(1, 1, 3, 1, 2)]),
            (
                "two items on two lines",
                "-  a\n-  b\n",
                &[(1, 1, 3, 1, 2), (2, 1, 3, 1, 2)],
            ),
            (
                "three spaces in a tight list is wrong",
                "-   a\n-   b\n",
                &[(1, 1, 4, 1, 3), (2, 1, 4, 1, 3)],
            ),
            ("one item, no trailing newline", "-   a", &[(1, 1, 4, 1, 3)]),
            (
                "a blank line between two one-line items",
                "-  a\n\n-  b\n",
                &[(1, 1, 3, 3, 2), (3, 1, 3, 3, 2)],
            ),
            (
                "three spaces satisfies multi but not single",
                "-   a\n\n-   b\n",
                &[],
            ),
            ("an item that wraps", "-  a\n  more\n", &[(1, 1, 3, 3, 2)]),
            (
                "three items, the middle wrapping",
                "-  a\n-  b\n  more\n-  c\n",
                &[(1, 1, 3, 3, 2), (2, 1, 3, 3, 2), (4, 1, 3, 3, 2)],
            ),
            (
                "an indented code block in the item",
                "-  a\n\n      code\n",
                &[(1, 1, 3, 3, 2)],
            ),
            (
                "a nested list",
                "- a\n  -  b\n",
                &[(1, 1, 2, 3, 1), (2, 3, 3, 1, 2)],
            ),
            (
                "a trailing blank line stays single",
                "-  a\n-  b\n\n",
                &[(1, 1, 3, 1, 2), (2, 1, 3, 1, 2)],
            ),
            (
                "two trailing blank lines stay single",
                "-  a\n-  b\n\n\n",
                &[(1, 1, 3, 1, 2), (2, 1, 3, 1, 2)],
            ),
            (
                "a trailing blank then prose stays single",
                "-  a\n-  b\n\ntext\n",
                &[(1, 1, 3, 1, 2), (2, 1, 3, 1, 2)],
            ),
            (
                "no trailing newline stays single",
                "-  a\n-  b",
                &[(1, 1, 3, 1, 2), (2, 1, 3, 1, 2)],
            ),
            (
                "a leading blank line stays single",
                "\n-  a\n-  b\n",
                &[(2, 1, 3, 1, 2), (3, 1, 3, 1, 2)],
            ),
            (
                "ordered lists split the same way",
                "1.  a\n\n2.  b\n",
                &[(1, 1, 4, 3, 2), (3, 1, 4, 3, 2)],
            ),
        ];
        check(cases, split_spacing_config());
    }
}
