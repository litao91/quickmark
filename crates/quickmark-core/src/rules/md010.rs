use crate::ast::Node;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

// MD010-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD010HardTabsTable {
    pub code_blocks: bool,
    pub ignore_code_languages: Vec<String>,
    /// markdownlint uses this to size the text its fix inserts. Nothing here emits fixes, so it
    /// changes nothing — but it stays in the schema, because a config written for markdownlint sets
    /// it.
    pub spaces_per_tab: usize,
}

impl Default for MD010HardTabsTable {
    fn default() -> Self {
        Self {
            code_blocks: true,
            ignore_code_languages: Vec::new(),
            spaces_per_tab: 1,
        }
    }
}

/// MD010 Hard Tabs Rule Linter
///
/// **SINGLE-USE CONTRACT**: This linter is designed for one-time use only.
/// After processing a document (via feed() calls and finalize()), the linter
/// should be discarded. The violations state is not cleared between uses.
pub(crate) struct MD010Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD010Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Analyze all lines and store all violations for reporting via finalize().
    /// Context cache is already initialized by MultiRuleLinter.
    fn analyze_all_lines(&mut self) {
        let settings = &self.context.config.linters.settings.hard_tabs;
        let lines = self.context.lines.borrow();

        // Determine which lines to exclude from hard tab checks.
        // If `code_blocks` is true (default), we check tabs in code blocks,
        // but may exclude specific languages via `ignore_code_languages`.
        // If `code_blocks` is false, we exclude all code blocks entirely.
        let excluded_lines = if settings.code_blocks {
            self.get_ignored_language_code_block_lines(settings)
        } else {
            self.get_all_code_block_lines()
        };

        for (line_index, line) in lines.iter().enumerate() {
            let line_number = line_index + 1;

            if excluded_lines.contains(&line_number) {
                continue;
            }

            // markdownlint matches /\t+/g, so a contiguous run of tabs is a single violation while
            // tabs separated by other content are reported separately.
            let mut previous_was_tab = false;
            for (char_index, ch) in line.char_indices() {
                let is_tab = ch == '\t';
                if is_tab && !previous_was_tab {
                    let violation = self.create_violation(line_index, char_index);
                    self.violations.push(violation);
                }
                previous_was_tab = is_tab;
            }
        }
    }

    /// Returns a set of line numbers from fenced code blocks where the language
    /// is in the user's ignore list (e.g., `ignore_code_languages = ["python"]`).
    fn get_ignored_language_code_block_lines(
        &self,
        settings: &crate::config::MD010HardTabsTable,
    ) -> HashSet<usize> {
        if settings.ignore_code_languages.is_empty() {
            return HashSet::new();
        }

        let node_cache = self.context.node_cache.borrow();
        let mut excluded_lines = HashSet::new();

        if let Some(fenced_code_blocks) = node_cache.get("fenced_code_block") {
            let lines = self.context.lines.borrow();
            for node_info in fenced_code_blocks {
                if let Some(first_line) = lines.get(node_info.line_start) {
                    if let Some(language) = self.extract_code_block_language(first_line) {
                        if settings.ignore_code_languages.contains(&language) {
                            for line_num in (node_info.line_start + 1)..=(node_info.line_end + 1) {
                                excluded_lines.insert(line_num);
                            }
                        }
                    }
                }
            }
        }

        excluded_lines
    }

    /// Returns a set of all line numbers that are part of any code block.
    fn get_all_code_block_lines(&self) -> HashSet<usize> {
        let node_cache = self.context.node_cache.borrow();
        ["indented_code_block", "fenced_code_block"]
            .iter()
            .filter_map(|kind| node_cache.get(*kind))
            .flatten()
            .flat_map(|node_info| (node_info.line_start + 1)..=(node_info.line_end + 1))
            .collect()
    }

    /// Extracts the language identifier from a fenced code block's info string.
    /// This handles common variations like attributes (e.g., ```rust{{...}}).
    fn extract_code_block_language(&self, line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") && !trimmed.starts_with("~~~") {
            return None;
        }

        let language_part = &trimmed[3..];
        language_part
            .split_whitespace()
            .next()
            // Handle language specifiers with attributes like ```rust{{...}}
            .map(|s| s.split('{').next().unwrap_or(s))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
    }

    /// Creates a RuleViolation for a hard tab at the specified position.
    fn create_violation(&self, line_index: usize, tab_position: usize) -> RuleViolation {
        let message = format!("{} [Column: {}]", MD010.description, tab_position + 1);

        RuleViolation::new(
            &MD010,
            message,
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                // FIXME: Byte offsets are not correctly calculated as line start offset is unavailable here.
                // This may result in incorrect highlighting in some tools.
                // The primary information is in the points (row/column).
                start_byte: 0,
                end_byte: 0,
                start_point: crate::ast::Point {
                    row: line_index,
                    column: tab_position,
                },
                end_point: crate::ast::Point {
                    row: line_index,
                    column: tab_position + 1,
                },
            }),
        )
    }
}

impl RuleLinter for MD010Linter {
    fn feed(&mut self, node: &Node) {
        // This rule is line-based and only needs to run once.
        // We trigger the analysis on seeing the top-level `document` node.
        if node.kind() == "document" {
            self.analyze_all_lines();
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD010: Rule = Rule {
    id: "MD010",
    aliases: &["no-hard-tabs"],
    tags: &["hard_tab", "whitespace"],
    description: "Hard tabs",
    rule_type: RuleType::Line,
    // This is a line-based rule and does not require specific nodes from the AST.
    // The logic runs once for the entire file content.
    required_nodes: &[],
    new_linter: |context| Box::new(MD010Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD010HardTabsTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::{test_config_with_rules, test_config_with_settings};

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-hard-tabs", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ])
    }

    fn test_config_with_hard_tabs(
        hard_tabs_config: MD010HardTabsTable,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![
                ("no-hard-tabs", RuleSeverity::Error),
                ("heading-style", RuleSeverity::Off),
                ("heading-increment", RuleSeverity::Off),
            ],
            LintersSettingsTable {
                hard_tabs: hard_tabs_config,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_basic_hard_tab_violation() {
        let input = "This line has a hard tab:\tafter this";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());

        let violation = &violations[0];
        assert_eq!("MD010", violation.rule().id);
        assert!(violation.message().contains("Hard tabs"));
    }

    #[test]
    fn test_no_hard_tabs() {
        let input = "This line has no hard tabs, only spaces.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_contiguous_tabs_are_one_violation() {
        let input = "\t\t\tindented";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // markdownlint matches /\t+/g, so a run counts once
        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_separated_tabs_are_separate_violations() {
        let input = "Line with\ttabs\tin\tmultiple places";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
    }

    #[test]
    fn test_hard_tabs_counted_per_run_across_lines() {
        let input = "first\tline\nno tabs\n\tsecond\tline";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
    }

    #[test]
    fn test_hard_tab_in_code_block_allowed_by_default() {
        let input = "```\nfunction example() {\n\treturn \"tab indented\";\n}\n```";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len()); // Code blocks should be checked by default
    }

    #[test]
    fn test_code_blocks_disabled() {
        let config = test_config_with_hard_tabs(MD010HardTabsTable {
            code_blocks: false,
            ignore_code_languages: Vec::new(),
            spaces_per_tab: 1,
        });

        let input = "```\nfunction example() {\n\treturn \"tab indented\";\n}\n```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len()); // Should not check code blocks when disabled
    }

    /// The line after a code block is ordinary text, so its tabs count even with `code_blocks` off.
    /// Every expectation is a markdownlint-cli2 v0.23.3 measurement.
    #[test]
    fn a_tab_after_a_code_block_counts() {
        for input in [
            "```\ncode\n```\ntext\tmore\n",
            "para\n\n    code\ntext\tmore\n",
        ] {
            let config = test_config_with_hard_tabs(MD010HardTabsTable {
                code_blocks: false,
                ignore_code_languages: Vec::new(),
                spaces_per_tab: 1,
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            let violations = linter.analyze();
            assert_eq!(1, violations.len(), "{input:?}");
            assert_eq!(
                4,
                violations[0].location().range.start.line + 1,
                "{input:?}"
            );
        }
    }

    #[test]
    fn test_ignore_specific_languages() {
        let config = test_config_with_hard_tabs(MD010HardTabsTable {
            code_blocks: true,
            ignore_code_languages: vec!["python".to_string()],
            spaces_per_tab: 1,
        });

        let input = "```python\ndef example():\n\treturn \"tab indented\"
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len()); // Should ignore python code blocks
    }

    #[test]
    fn test_custom_spaces_per_tab() {
        let config = test_config_with_hard_tabs(MD010HardTabsTable {
            code_blocks: true,
            ignore_code_languages: Vec::new(),
            spaces_per_tab: 4,
        });

        let input = "Line with\thard tab";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());

        let violation = &violations[0];
        // markdownlint names the column, not the setting; `spaces_per_tab` only sizes the text its
        // fix inserts, and this emits no fixes.
        assert_eq!("Hard tabs [Column: 10]", violation.message());
    }

    #[test]
    fn test_indented_code_block() {
        let input = "    This is indented code with\ttab";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len()); // Should still flag tabs in indented code blocks by default
    }

    #[test]
    fn test_multiple_lines_mixed() {
        let input = r###"Line without tabs
Line with	tab
Another normal line
Another	line	with	tabs"###;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(4, violations.len()); // Should report violations for each tab (1 + 3 tabs)
    }
}
