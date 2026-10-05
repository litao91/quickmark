use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

/// MD056 - Table column count
///
/// This rule checks that all rows in a table have the same number of columns.
pub(crate) struct MD056Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD056Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    fn check_table_column_count(&mut self, table_node: &Node) {
        let mut cursor = table_node.walk();
        let mut table_rows = table_node.children(&mut cursor).filter(|child| {
            matches!(
                child.kind(),
                "pipe_table_header" | "pipe_table_row" | "pipe_table_delimiter_row"
            )
        });

        let Some(first_row) = table_rows.next() else {
            return;
        };

        let expected_column_count = self.count_table_cells(&first_row);

        // The first row determines the expected count, so we only need to check subsequent rows.
        for row in table_rows {
            let actual_column_count = self.count_table_cells(&row);

            if actual_column_count == expected_column_count {
                continue;
            }

            // markdownlint's `errorRange` is `[row.endColumn - 1, 1]` for a short row and
            // `[cells[expected].startColumn, row.endColumn - cells[expected].startColumn]` for a long
            // one. Both are measured against the row's whole line — micromark's `tableRow` runs to
            // the line's last byte before its terminator, trailing whitespace included — and a long
            // row is named by the divider before its first excess cell, not by the cell's own text.
            let line_len = self.row_line_len(&row);
            let (message, start, length) = if actual_column_count < expected_column_count {
                (
                    format!(
                        "{} [Expected: {expected_column_count}; Actual: {actual_column_count}; Too few cells, row will be missing data]",
                        MD056.description
                    ),
                    line_len - 1,
                    1,
                )
            } else {
                let start = self.get_extra_cells_position(&row, expected_column_count);
                (
                    format!(
                        "{} [Expected: {expected_column_count}; Actual: {actual_column_count}; Too many cells, extra data will be missing]",
                        MD056.description
                    ),
                    start,
                    line_len - start,
                )
            };

            let mut range = range_from_node_range(&row.range());
            range.start.character = start;
            range.end.character = start + length;

            self.violations.push(RuleViolation::new(
                &MD056,
                message,
                self.context.file_path.clone(),
                range,
            ));
        }
    }

    fn count_table_cells(&self, row_node: &Node) -> usize {
        row_node
            .children(&mut row_node.walk())
            .filter(|child| {
                matches!(
                    child.kind(),
                    "pipe_table_cell" | "pipe_table_delimiter_cell"
                )
            })
            .count()
    }

    /// The length of the row's own line, excluding its terminator — markdownlint's `row.endColumn`
    /// minus one.
    ///
    /// The row node ends at the line's last non-blank byte, so it cannot answer this: micromark's
    /// `tableRow` runs the whole line and `row.endColumn` counts trailing whitespace too.
    fn row_line_len(&self, row_node: &Node) -> usize {
        self.context.lines.borrow()[row_node.start_position().row].len()
    }

    /// The column of the divider before the first cell past the expected count.
    ///
    /// markdownlint takes the column from micromark's `tableData` token, and that token starts at the
    /// `tableCellDivider` before the cell rather than at the cell's own text, so it names the pipe.
    /// A row without a leading pipe has no divider before its first cell, but the first cell is
    /// never the excess one — the expected count is at least one.
    fn get_extra_cells_position(&self, row_node: &Node, expected_count: usize) -> usize {
        let mut cursor = row_node.walk();
        let mut divider = None;
        let mut cells = 0;
        for child in row_node.children(&mut cursor) {
            match child.kind() {
                "|" => divider = Some(child),
                "pipe_table_cell" | "pipe_table_delimiter_cell" => {
                    if cells == expected_count {
                        return divider.unwrap_or(child).start_position().column;
                    }
                    cells += 1;
                }
                _ => {}
            }
        }
        0
    }
}

impl RuleLinter for MD056Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "pipe_table" {
            self.check_table_column_count(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD056: Rule = Rule {
    id: "MD056",
    aliases: &["table-column-count"],
    tags: &["table"],
    description: "Table column count",
    rule_type: RuleType::Token,
    required_nodes: &["pipe_table"],
    new_linter: |context| Box::new(MD056Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::{
        config::RuleSeverity, linter::MultiRuleLinter,
        test_utils::test_helpers::test_config_with_rules,
    };

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("table-column-count", RuleSeverity::Error)])
    }

    /// A case's name, its document, and the `(line, column, width)` of each report markdownlint
    /// makes on it, all 1-based.
    type Case = (&'static str, &'static str, &'static [(usize, usize, usize)]);

    fn ranges(input: &str) -> Vec<(usize, usize, usize)> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
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

    #[test]
    fn test_table_with_consistent_column_count() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
| Cell 3   | Cell 4   |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_with_too_few_cells() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
| Cell 3   |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("Too few cells"));
    }

    #[test]
    fn test_table_with_too_many_cells() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
| Cell 3   | Cell 4   | Cell 5 |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("Too many cells"));
    }

    /// markdownlint names the divider before the first excess cell — micromark's `tableData` token
    /// starts at its `tableCellDivider` — and underlines from there to the end of the row's *line*,
    /// trailing whitespace included; a short row gets that line's last byte, one column wide.
    /// Every tuple is markdownlint v0.41.1's `errorRange`, read back through its `lintSync` API.
    #[test]
    fn matches_markdownlints_error_range() {
        let cases: &[Case] = &[
            (
                "one excess cell",
                "| a | b |\n| - | - |\n| 1 | 2 | 3 |\n",
                &[(3, 9, 5)],
            ),
            // The report starts at the divider even though the cell's own text is further right.
            (
                "padded excess cell",
                "| a | b |\n| - | - |\n| 1 | 2 |   3   |\n",
                &[(3, 9, 9)],
            ),
            (
                "two excess cells",
                "| a | b |\n| - | - |\n| 1 | 2 | 3 | 4 |\n",
                &[(3, 9, 9)],
            ),
            (
                "an escaped backslash before a real delimiter",
                "| a | b |\n| - | - |\n| m \\\\| n | q |\n",
                &[(3, 11, 5)],
            ),
            (
                "a row that matches",
                "| a | b |\n| - | - |\n| 1 | 2 |\n",
                &[],
            ),
            (
                "long, longer and short rows together",
                "| a | b |\n| - | - |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 | 7 |\n| 8 |\n",
                &[(3, 9, 5), (4, 9, 9), (5, 5, 1)],
            ),
            // The column counts from the line's start, so a row's own indent is inside it.
            (
                "an indented row",
                "| a | b |\n| - | - |\n | 1 | 2 | 3 |\n",
                &[(3, 10, 5)],
            ),
            (
                "a row indented three",
                "| a | b |\n| - | - |\n   | 1 | 2 | 3 |\n",
                &[(3, 12, 5)],
            ),
            (
                "an indented short row",
                "| a | b | c |\n| - | - | - |\n | 1 | 2 |\n",
                &[(3, 10, 1)],
            ),
            (
                "a short row indented three",
                "| a | b | c |\n| - | - | - |\n   | 1 | 2 |\n",
                &[(3, 12, 1)],
            ),
            // ...and so is a block quote's marker, which is part of the line micromark measures.
            (
                "a quoted row",
                "> | a | b |\n> | - | - |\n> | 1 | 2 | 3 |\n",
                &[(3, 11, 5)],
            ),
            (
                "a quoted short row",
                "> | a | b | c |\n> | - | - | - |\n> | 1 | 2 |\n",
                &[(3, 11, 1)],
            ),
            // Trailing whitespace is inside micromark's `tableRow`, so it is inside the width, and
            // a short row's last byte is a space rather than the pipe.
            (
                "trailing spaces",
                "| a | b |\n| - | - |\n| 1 | 2 | 3 |   \n",
                &[(3, 9, 8)],
            ),
            (
                "a trailing tab",
                "| a | b |\n| - | - |\n| 1 | 2 | 3 |\t\n",
                &[(3, 9, 6)],
            ),
            (
                "trailing spaces on a short row",
                "| a | b | c |\n| - | - | - |\n| 1 | 2 |   \n",
                &[(3, 12, 1)],
            ),
            (
                "a tab and a space on a short row",
                "| a | b | c |\n| - | - | - |\n| 1 | 2 |\t \n",
                &[(3, 11, 1)],
            ),
            // `\r` is part of the line terminator, not of the row.
            (
                "crlf",
                "| a | b |\r\n| - | - |\r\n| 1 | 2 | 3 |\r\n",
                &[(3, 9, 5)],
            ),
            (
                "crlf with trailing spaces",
                "| a | b |\r\n| - | - |\r\n| 1 | 2 | 3 |  \r\n",
                &[(3, 9, 7)],
            ),
            // Four columns of indent make the row indented code, so there is no table to count.
            (
                "a tab-indented row is code",
                "| a | b |\n| - | - |\n\t| 1 | 2 | 3 |\n",
                &[],
            ),
        ];
        for (name, source, expected) in cases {
            assert_eq!(*expected, ranges(source).as_slice(), "{name}");
        }

        // A short row is reported on its own last byte, one column wide.
        assert_eq!(
            vec![(3, 9, 1), (4, 9, 1)],
            ranges("| a | b | c |\n| - | - | - |\n| 1 | 2 |\n| 3 | 4 |\n")
        );
        // A grid with no outer pipes is not a table to either parser, so it has no column count.
        assert_eq!(0, ranges("a | b\n- | -\n1 | 2 | 3\n").len());
    }

    #[test]
    fn test_table_with_mixed_column_counts() {
        let input = r#"| Header 1 | Header 2 | Header 3 |
| -------- | -------- | -------- |
| Cell 1   | Cell 2   |
| Cell 3   | Cell 4   | Cell 5   | Cell 6 |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len());
        assert!(violations[0].message().contains("Too few cells"));
        assert!(violations[1].message().contains("Too many cells"));
    }

    #[test]
    fn test_table_header_only() {
        let input = r#"| Header 1 | Header 2 |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_with_delimiter_row_only() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_empty_cells_in_table() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
|          | Cell 2   |
| Cell 3   |          |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_with_one_column() {
        let input = r#"| Header |
| ------ |
| Cell 1 |
| Cell 2 |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_with_one_column_violation() {
        let input = r#"| Header |
| ------ |
| Cell 1 | Cell 2 |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("Too many cells"));
    }

    #[test]
    fn test_multiple_tables_independent() {
        let input = r#"| Table 1 | Header |
| ------- | ------ |
| Cell    | Value  |

| Different | Table | Headers |
| --------- | ----- | ------- |
| More      | Data  | Here    |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_multiple_tables_with_violations() {
        let input = r#"| Table 1 | Header |
| ------- | ------ |
| Cell    |

| Different | Table |
| --------- | ----- |
| More      | Data  | Extra |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len());
        assert!(violations[0].message().contains("Too few cells"));
        assert!(violations[1].message().contains("Too many cells"));
    }
}
