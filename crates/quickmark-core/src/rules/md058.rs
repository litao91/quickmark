use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{Rule, RuleType},
};

/// MD058 - Tables should be surrounded by blank lines
///
/// This rule checks that tables have blank lines before and after them,
/// except when the table is at the very beginning or end of the document.
pub(crate) struct MD058Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD058Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    fn check_table_blanks(&mut self, table_node: &Node) {
        let start_line = table_node.start_position().row;
        let lines = self.context.lines.borrow();

        // GFM absorbs a following pipe-less line into the table as a one-cell row, so the table's
        // last row is the right thing to look past for the blank line below. Filtering for rows that
        // contain a pipe — which is what this did under tree-sitter-md, whose table ended above the
        // absorbed line — reports a violation markdownlint does not.
        let mut cursor = table_node.walk();
        let Some(last_row) = table_node.children(&mut cursor).last() else {
            return; // No rows in table, nothing to check.
        };

        let actual_end_line = last_row.end_position().row;

        // Check for a blank line above the table if it's not at the document start.
        if start_line > 0 {
            // A blank line is required only if there is non-blank content somewhere above the table.
            let has_content_above = (0..start_line).any(|i| !lines[i].trim().is_empty());

            if has_content_above && !lines[start_line - 1].trim().is_empty() {
                self.violations.push(RuleViolation::new(
                    &MD058,
                    format!("{} [Above]", MD058.description),
                    self.context.file_path.clone(),
                    range_from_node_range(&table_node.range()),
                ));
            }
        }

        // Check for a blank line below the table if it's not at the document end.
        if actual_end_line + 1 < lines.len() {
            // A blank line is required only if there is non-blank content somewhere below the table.
            let has_content_below =
                ((actual_end_line + 1)..lines.len()).any(|i| !lines[i].trim().is_empty());

            if has_content_below && !lines[actual_end_line + 1].trim().is_empty() {
                self.violations.push(RuleViolation::new(
                    &MD058,
                    format!("{} [Below]", MD058.description),
                    self.context.file_path.clone(),
                    range_from_node_range(&table_node.range()),
                ));
            }
        }
    }
}

impl RuleLinter for MD058Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "pipe_table" {
            self.check_table_blanks(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD058: Rule = Rule {
    id: "MD058",
    alias: "blanks-around-tables",
    tags: &["table", "blank_lines"],
    description: "Tables should be surrounded by blank lines",
    rule_type: RuleType::Token,
    required_nodes: &["pipe_table"],
    new_linter: |context| Box::new(MD058Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::{
        config::RuleSeverity, linter::MultiRuleLinter,
        test_utils::test_helpers::test_config_with_rules,
    };

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("blanks-around-tables", RuleSeverity::Error)])
    }

    #[test]
    fn test_table_with_proper_blank_lines() {
        let input = r#"Some text

| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |

More text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_missing_blank_line_above() {
        let input = r#"Some text
| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |

More text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("[Above]"));
    }

    #[test]
    fn test_table_missing_blank_line_below() {
        let input = r#"Some text

| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
# Heading"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("[Below]"));
    }

    /// GFM absorbs a pipe-less line following a table as a one-cell row, so plain text under a table
    /// does not end it and there is no "below" to check. markdownlint agrees: 0 violations here.
    #[test]
    fn test_table_absorbs_following_text_line() {
        let input = r#"Some text

| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
More text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_missing_both_blank_lines() {
        let input = r#"Some text
| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |
# Heading"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len());
        assert!(violations[0].message().contains("[Above]"));
        assert!(violations[1].message().contains("[Below]"));
    }

    #[test]
    fn test_table_at_start_of_document() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |

More text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // Should not violate - no content above to require blank line
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_at_end_of_document() {
        let input = r#"Some text

| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // Should not violate - no content below to require blank line
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_table_alone_in_document() {
        let input = r#"| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // Should not violate - no content above or below
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_multiple_tables_proper_spacing() {
        let input = r#"Some text

| Table 1 | Header |
| ------- | ------ |
| Cell    | Value  |

Text between tables

| Table 2 | Header |
| ------- | ------ |
| Cell    | Value  |

Final text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    /// No blank lines anywhere, so GFM keeps absorbing rows and this is a single table running to the
    /// end of the document — one missing blank line above, nothing below it to check. markdownlint
    /// reports 1 here too; tree-sitter-md ended the table at each pipe-less line and saw four.
    #[test]
    fn test_tables_with_no_blank_lines_are_one_table() {
        let input = r#"Some text
| Table 1 | Header |
| ------- | ------ |
| Cell    | Value  |
Text between tables
| Table 2 | Header |
| ------- | ------ |
| Cell    | Value  |
Final text"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("[Above]"));
    }

    /// Headings do end a table, so these really are two tables and all four sides are checked.
    #[test]
    fn test_multiple_tables_improper_spacing() {
        let input = r#"Some text
| Table 1 | Header |
| ------- | ------ |
| Cell    | Value  |
# Between
| Table 2 | Header |
| ------- | ------ |
| Cell    | Value  |
# End
"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(4, violations.len());
        assert!(violations[0].message().contains("[Above]"));
        assert!(violations[1].message().contains("[Below]"));
        assert!(violations[2].message().contains("[Above]"));
        assert!(violations[3].message().contains("[Below]"));
    }

    #[test]
    fn test_table_with_only_blank_lines_above_and_below() {
        let input = r#"


| Header 1 | Header 2 |
| -------- | -------- |
| Cell 1   | Cell 2   |


"#;
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // Should not violate - no actual content above or below
        assert_eq!(0, violations.len());
    }
}
