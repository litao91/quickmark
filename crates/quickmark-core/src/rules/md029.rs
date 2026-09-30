use serde::Deserialize;
use std::rc::Rc;

use tree_sitter::Node;

use crate::{
    linter::{range_from_tree_sitter, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

// MD029-specific configuration types
#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
pub enum OlPrefixStyle {
    #[serde(rename = "one")]
    One,
    #[serde(rename = "ordered")]
    Ordered,
    #[serde(rename = "one_or_ordered")]
    OneOrOrdered,
    #[serde(rename = "zero")]
    Zero,
}

impl Default for OlPrefixStyle {
    fn default() -> Self {
        Self::OneOrOrdered
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD029OlPrefixTable {
    #[serde(default)]
    pub style: OlPrefixStyle,
}

impl Default for MD029OlPrefixTable {
    fn default() -> Self {
        Self {
            style: OlPrefixStyle::OneOrOrdered,
        }
    }
}

pub(crate) struct MD029Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    // Document-wide state for one_or_ordered mode
    document_style: Option<OlPrefixStyle>,
    // Whether the document uses zero-based ordering (0,1,2...) vs one-based (1,2,3...)
    is_zero_based: bool,
}

impl MD029Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            document_style: None,
            is_zero_based: false,
        }
    }

    /// Extract the numeric value from an ordered list item prefix
    fn extract_list_item_value(&self, list_item_node: &Node) -> Option<u32> {
        let content = self.context.document_content.borrow();
        let source_bytes = content.as_bytes();

        // Find the list marker within this list item
        let mut cursor = list_item_node.walk();
        let result = list_item_node
            .children(&mut cursor)
            .find(|child| child.kind() == "list_marker_dot")
            .and_then(|marker_node| marker_node.utf8_text(source_bytes).ok())
            .and_then(|text| text.trim().trim_end_matches('.').parse::<u32>().ok());
        result
    }

    /// Check if a list node is an ordered list by examining its first marker.
    /// This is an optimization based on the assumption that a list is either
    /// entirely ordered or unordered.
    fn is_ordered_list(&self, list_node: &Node) -> bool {
        let mut cursor = list_node.walk();
        if let Some(first_item) = list_node
            .children(&mut cursor)
            .find(|c| c.kind() == "list_item")
        {
            let mut item_cursor = first_item.walk();
            return first_item
                .children(&mut item_cursor)
                .any(|child| child.kind() == "list_marker_dot");
        }
        false
    }

    /// Get style examples for error messages
    fn get_style_example(&self, style: &OlPrefixStyle) -> &'static str {
        match style {
            OlPrefixStyle::One => "1/1/1",
            OlPrefixStyle::Ordered => {
                if self.is_zero_based {
                    "0/1/2"
                } else {
                    "1/2/3"
                }
            }
            OlPrefixStyle::OneOrOrdered => "1/1/1 or 1/2/3",
            OlPrefixStyle::Zero => "0/0/0",
        }
    }

    fn check_list(&mut self, node: &Node) {
        let configured_style = self.context.config.linters.settings.ol_prefix.style;

        // Extract list items and their values with position information
        let mut list_items_with_values = Vec::new();
        let mut cursor = node.walk();

        for list_item in node.children(&mut cursor) {
            if list_item.kind() == "list_item" {
                if let Some(value) = self.extract_list_item_value(&list_item) {
                    list_items_with_values.push((list_item, value));
                }
            }
        }

        if list_items_with_values.is_empty() {
            return; // No items, nothing to check
        }

        // Split the continuous list into logical separate lists like markdownlint.
        // Performance: Collect lines once to avoid re-iterating the whole document content
        // for each list item pair.
        let logical_lists = {
            let content = self.context.document_content.borrow();
            let lines: Vec<&str> = content.lines().collect();
            self.split_into_logical_lists(&list_items_with_values, &lines)
        };

        for logical_list in logical_lists {
            match configured_style {
                OlPrefixStyle::OneOrOrdered => {
                    self.check_list_one_or_ordered(&logical_list);
                }
                OlPrefixStyle::One => {
                    self.check_list_with_fixed_style(&logical_list, OlPrefixStyle::One);
                }
                OlPrefixStyle::Zero => {
                    self.check_list_with_fixed_style(&logical_list, OlPrefixStyle::Zero);
                }
                OlPrefixStyle::Ordered => {
                    self.check_list_with_fixed_style(&logical_list, OlPrefixStyle::Ordered);
                }
            }
        }
    }

    /// Split tree-sitter's continuous list into logical separate lists like markdownlint
    fn split_into_logical_lists<'a>(
        &self,
        list_items_with_values: &[(Node<'a>, u32)],
        lines: &[&str],
    ) -> Vec<Vec<(Node<'a>, u32)>> {
        if list_items_with_values.len() <= 1 {
            return vec![list_items_with_values.to_vec()];
        }

        let mut logical_lists = Vec::new();
        let mut current_list = Vec::new();

        for (i, (list_item, value)) in list_items_with_values.iter().enumerate() {
            current_list.push((*list_item, *value));

            // Check if this should end the current logical list
            let should_split = if i < list_items_with_values.len() - 1 {
                let current_start_line = list_item.start_position().row;
                let next_start_line = list_items_with_values[i + 1].0.start_position().row;

                let lines_between = if (current_start_line + 1) < next_start_line {
                    &lines[(current_start_line + 1)..next_start_line]
                } else {
                    &[]
                };

                let (has_content_separation, has_blank_lines) =
                    self.analyze_lines_between(lines_between.iter().copied());
                let has_numbering_gap =
                    self.has_significant_numbering_gap(*value, list_items_with_values[i + 1].1);

                // Split if there's content separation OR (blank lines AND significant numbering gap)
                has_content_separation || (has_blank_lines && has_numbering_gap)
            } else {
                false
            };

            if should_split {
                logical_lists.push(std::mem::take(&mut current_list));
            }
        }

        // Add the final list if it has items
        if !current_list.is_empty() {
            logical_lists.push(current_list);
        }

        logical_lists
    }

    /// Check for content or blank lines between list items in a single pass.
    /// Returns a tuple: (has_content_separation, has_blank_lines).
    fn analyze_lines_between<'b, I>(&self, lines: I) -> (bool, bool)
    where
        I: Iterator<Item = &'b str>,
    {
        let mut has_blank_lines = false;
        let mut has_content_separation = false;

        for line in lines {
            let trimmed_line = line.trim();

            if trimmed_line.is_empty() {
                has_blank_lines = true;
                continue;
            }

            // Check for separating content
            if trimmed_line.starts_with('#') // Heading
                || trimmed_line.starts_with("---") // Horizontal rule
                || trimmed_line.starts_with("***")
            {
                has_content_separation = true;
                break; // Found separation, no need to check further
            }

            // Any other non-indented content also separates lists
            if !line.starts_with(' ') && !line.starts_with('\t') {
                has_content_separation = true;
                break; // Found separation
            }
        }

        (has_content_separation, has_blank_lines)
    }

    /// Check if there's a significant numbering gap between two list items
    /// A gap of more than 1 is considered significant (e.g., 2 -> 100)
    fn has_significant_numbering_gap(&self, current: u32, next: u32) -> bool {
        // If next number is not the immediate successor, it's a significant gap
        next != current + 1
    }

    /// Validate a list under the `one_or_ordered` style.
    ///
    /// markdownlint decides the style per list, never per document: a list is "ordered" when its
    /// second item is not 1 (or it starts at 0), and "one" otherwise. Latching a document-wide
    /// style from the first multi-item list makes an early all-`1.` list condemn every later
    /// 1/2/3 list.
    fn check_list_one_or_ordered(&mut self, list_items_with_values: &[(Node, u32)]) {
        let shows_numbering_pattern = list_items_with_values.len() >= 2;
        if shows_numbering_pattern {
            let first_value = list_items_with_values[0].1;
            let second_value = list_items_with_values[1].1;

            if second_value != 1 || first_value == 0 {
                self.document_style = Some(OlPrefixStyle::Ordered);
                self.is_zero_based = first_value == 0;
            } else {
                self.document_style = Some(OlPrefixStyle::One);
                self.is_zero_based = false;
            }
        } else {
            // A single item shows no pattern, so markdownlint holds it to "1".
            self.document_style = Some(OlPrefixStyle::One);
            self.is_zero_based = false;
        }

        let effective_style = self.document_style.unwrap_or(OlPrefixStyle::One);

        // Each logical list is validated against the style it itself established
        match effective_style {
            OlPrefixStyle::One => {
                // One style: all items should be "1."
                for (list_item, actual_value) in list_items_with_values {
                    if actual_value != &1 {
                        let message = format!(
                            "{} [Expected: 1; Actual: {}; Style: {}]",
                            MD029.description,
                            actual_value,
                            self.get_style_example(&effective_style)
                        );

                        self.violations.push(RuleViolation::new(
                            &MD029,
                            message,
                            self.context.file_path.clone(),
                            range_from_tree_sitter(&list_item.range()),
                        ));
                    }
                }
            }
            OlPrefixStyle::Ordered => {
                // Ordered style: in one_or_ordered mode, handle first list vs separated lists differently
                if !list_items_with_values.is_empty() {
                    let list_start_value = list_items_with_values[0].1;

                    // markdownlint starts an ordered list at 1, except when the list itself begins
                    // at 0, which makes it a 0/1/2 sequence. Nothing is remembered across lists.
                    let expected_start = if list_start_value == 0 { 0 } else { 1 };

                    // Check if the first item in this logical list starts with the correct value
                    let mut expected_value = expected_start;

                    for (list_item, actual_value) in list_items_with_values {
                        if actual_value != &expected_value {
                            let message = format!(
                                "{} [Expected: {}; Actual: {}; Style: {}]",
                                MD029.description,
                                expected_value,
                                actual_value,
                                self.get_style_example(&effective_style)
                            );

                            self.violations.push(RuleViolation::new(
                                &MD029,
                                message,
                                self.context.file_path.clone(),
                                range_from_tree_sitter(&list_item.range()),
                            ));
                        }
                        expected_value += 1;
                    }
                }
            }
            _ => {} // Other styles not relevant here
        }
    }

    fn check_list_with_fixed_style(
        &mut self,
        list_items_with_values: &[(Node, u32)],
        style: OlPrefixStyle,
    ) {
        // For fixed styles, each list is independent (original behavior)
        if list_items_with_values.len() < 2 {
            return; // Single item lists are always valid
        }

        let (effective_style, mut expected_value) = match style {
            OlPrefixStyle::One => (OlPrefixStyle::One, 1),
            OlPrefixStyle::Zero => (OlPrefixStyle::Zero, 0),
            OlPrefixStyle::Ordered => (OlPrefixStyle::Ordered, list_items_with_values[0].1),
            OlPrefixStyle::OneOrOrdered => unreachable!(), // Handled separately
        };

        // Check each list item against the expected pattern
        for (list_item, actual_value) in list_items_with_values {
            let should_report = match effective_style {
                OlPrefixStyle::One => actual_value != &1,
                OlPrefixStyle::Zero => actual_value != &0,
                OlPrefixStyle::Ordered => actual_value != &expected_value,
                OlPrefixStyle::OneOrOrdered => unreachable!(),
            };

            if should_report {
                let message = format!(
                    "{} [Expected: {}; Actual: {}; Style: {}]",
                    MD029.description,
                    expected_value,
                    actual_value,
                    self.get_style_example(&effective_style)
                );

                self.violations.push(RuleViolation::new(
                    &MD029,
                    message,
                    self.context.file_path.clone(),
                    range_from_tree_sitter(&list_item.range()),
                ));
            }

            // For ordered style, increment expected value (within this list only)
            if matches!(effective_style, OlPrefixStyle::Ordered) {
                expected_value += 1;
            }
        }
    }
}

impl RuleLinter for MD029Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "list" && self.is_ordered_list(node) {
            self.check_list(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD029: Rule = Rule {
    id: "MD029",
    alias: "ol-prefix",
    tags: &["ol"],
    description: "Ordered list item prefix",
    rule_type: RuleType::Document,
    required_nodes: &["list"],
    new_linter: |context| Box::new(MD029Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{
        LintersSettingsTable, LintersTable, MD029OlPrefixTable, OlPrefixStyle, QuickmarkConfig,
        RuleSeverity,
    };
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;
    use std::collections::HashMap;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("ol-prefix", RuleSeverity::Error)])
    }

    fn test_config_style(style: OlPrefixStyle) -> crate::config::QuickmarkConfig {
        let severity: HashMap<String, RuleSeverity> =
            vec![("ol-prefix".to_string(), RuleSeverity::Error)]
                .into_iter()
                .collect();

        QuickmarkConfig::new(LintersTable {
            severity,
            settings: LintersSettingsTable {
                ol_prefix: MD029OlPrefixTable { style },
                ..Default::default()
            },
        })
    }

    #[test]
    fn test_empty_document() {
        let input = "";
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_single_item_separated_lists_start_at_one() {
        // Edge case discovered during parity testing:
        // Single-item lists separated by content should each start at 1
        let input = "# Test\n\n1. Single item\n\ntext\n\n2. This should be 1\n\ntext\n\n3. This should also be 1\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(
            2,
            violations.len(),
            "Single-item separated lists should start at 1"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 2"));
        assert!(violations[1].message().contains("Expected: 1; Actual: 3"));
    }

    #[test]
    fn test_separated_lists_proper_numbering() {
        // Edge case: Separated lists should start fresh, not continue from previous
        let input = "# First List\n\n1. First\n2. Second\n3. Third\n\n# Second List\n\n4. Should be 1\n5. Should be 2\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(
            2,
            violations.len(),
            "Separated lists should start fresh at 1"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 4"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 5"));
        // Should detect as ordered style from first list
        assert!(violations[0].message().contains("Style: 1/2/3"));
    }

    /// Lint with the default `one_or_ordered` style and return the violation messages.
    fn one_or_ordered(input: &str) -> Vec<String> {
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        violations.iter().map(|v| v.message().to_string()).collect()
    }

    #[test]
    fn test_ones_list_then_ordered_list_are_both_valid() {
        // markdownlint judges every list on its own, so an all-`1.` list says nothing about a
        // later 1/2/3 list. This used to latch a document-wide "one" style and flag the second.
        let input = "# First (sets style)\n\n1. One\n1. One\n1. One\n\n# Second (must follow)\n\n1. Should pass\n2. Should violate\n3. Should violate\n";
        assert!(one_or_ordered(input).is_empty());
    }

    #[test]
    fn test_ordered_list_then_ones_list_are_both_valid() {
        // The mirror image: an ordered list first does not force a later all-`1.` list to increment.
        let input = "# First (sets ordered style)\n\n1. First\n2. Second\n3. Third\n\n# Second (violates)\n\n1. Should violate\n1. Should violate\n";
        assert!(one_or_ordered(input).is_empty());
    }

    #[test]
    fn test_zero_based_continuous_list_valid() {
        // Edge case: 0,1,2 should be valid zero-based continuous list
        let input = "# Test\n\n0. Zero start\n1. One\n2. Two\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(
            0,
            violations.len(),
            "Zero-based continuous list should be valid"
        );
    }

    #[test]
    fn test_zero_based_lists_are_valid_independently() {
        // A 0/1/2 list is "ordered" starting at 0, and a later 0/1 list is judged the same way on
        // its own rather than being forced to start at 1 by the earlier list.
        let input = "# First (zero-based)\n\n0. Zero\n1. One\n2. Two\n\n# Second (should start at 1)\n\n0. Should violate\n1. Should violate\n";
        assert!(one_or_ordered(input).is_empty());
    }

    #[test]
    fn test_mixed_single_and_multi_item_lists() {
        // Only the single-item list that does not start at 1 is wrong; the all-`1.` list and the
        // 1/2 list are each valid in their own right.
        let input = "# Mix test\n\n5. Single wrong start\n\ntext\n\n1. Multi start\n1. Multi second\n1. Multi third\n\ntext\n\n1. Single correct\n\ntext\n\n1. Multi after\n2. Should violate (ones style established)\n";
        let messages = one_or_ordered(input);
        assert_eq!(1, messages.len(), "unexpected: {messages:?}");
        assert!(messages[0].contains("Expected: 1; Actual: 5"));
    }

    #[test]
    fn test_large_numbers_separated_lists() {
        // Both lists increment, so both are "ordered" — and an ordered list starts at 1 (or 0),
        // never at an arbitrary number. Three violations for 98/99/100, two for 200/201.
        let input = "# First\n\n98. Large start\n99. Large next\n100. Large third\n\n# Second\n\n200. Should be 1\n201. Should be 2\n";
        let messages = one_or_ordered(input);
        assert_eq!(5, messages.len(), "unexpected: {messages:?}");
        assert!(messages.iter().any(|m| m.contains("Actual: 98")));
        assert!(messages.iter().any(|m| m.contains("Actual: 200")));
        assert!(messages.iter().all(|m| m.contains("Style: 1/2/3")));
    }

    #[test]
    fn test_nested_lists_do_not_inherit_outer_style() {
        // A nested 1/2 list inside an all-`1.` parent is ordered in its own right, and so is a
        // later 1/2 list at the top level.
        let input = "# Test\n\n1. Parent one\n1. Parent one\n   1. Nested ordered\n   2. Nested ordered (violates 'one' style)\n1. Parent one\n\n# Separate\n\n1. Should not violate\n2. Should violate (violates 'one' style)\n";
        assert!(one_or_ordered(input).is_empty());
    }

    #[test]
    fn test_empty_lines_vs_content_separation() {
        // Edge case: Ensure proper distinction between blank line separation and content separation
        let input = "# Test\n\n1. First list\n2. Second item\n\n\n3. After blank lines - should continue\n\nActual content\n\n1. New list - should start at 1\n2. Second in new list\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Blank lines alone shouldn't separate, but content should
        assert_eq!(
            0,
            violations.len(),
            "Blank lines alone shouldn't separate lists, content should create new list"
        );
    }

    #[test]
    fn test_single_item_list() {
        let input = "1. Single item\n";
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Single item lists should not violate");
    }

    #[test]
    fn test_no_ordered_lists() {
        let input = "* Unordered item\n- Another item\n+ Plus item\n";
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Should not check unordered lists");
    }

    // Test one_or_ordered style (default)
    #[test]
    fn test_one_or_ordered_detects_one_style() {
        let input = "1. Item one\n1. Item two\n1. Item three\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Should detect and allow 'one' style");
    }

    #[test]
    fn test_one_or_ordered_detects_ordered_style() {
        let input = "1. Item one\n2. Item two\n3. Item three\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Should detect and allow 'ordered' style"
        );
    }

    #[test]
    fn test_one_or_ordered_detects_zero_based() {
        let input = "0. Item zero\n1. Item one\n2. Item two\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Should detect and allow zero-based ordered style"
        );
    }

    #[test]
    fn test_one_or_ordered_violates_mixed_style() {
        let input = "1. Item one\n1. Item two\n3. Item three\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len(), "Should violate inconsistent numbering");
        // In "one_or_ordered" mode with pattern 1/1/3, this should be detected as "one" style
        // So the violation should be that item 3 has "3" instead of "1"
        assert!(violations[0].message().contains("Expected: 1; Actual: 3"));
    }

    // Test "one" style
    #[test]
    fn test_one_style_passes() {
        let input = "1. Item one\n1. Item two\n1. Item three\n";
        let config = test_config_style(OlPrefixStyle::One);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "All '1.' should pass");
    }

    #[test]
    fn test_one_style_violates_ordered() {
        let input = "1. Item one\n2. Item two\n3. Item three\n";
        let config = test_config_style(OlPrefixStyle::One);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len(), "Should violate items 2 and 3");
        assert!(violations[0].message().contains("Expected: 1; Actual: 2"));
        assert!(violations[1].message().contains("Expected: 1; Actual: 3"));
    }

    #[test]
    fn test_one_style_violates_zero_start() {
        let input = "0. Item zero\n1. Item one\n2. Item two\n";
        let config = test_config_style(OlPrefixStyle::One);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            2,
            violations.len(),
            "Should violate items with 0 and 2, but not 1"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 0"));
        assert!(violations[1].message().contains("Expected: 1; Actual: 2"));
    }

    // Test "ordered" style
    #[test]
    fn test_ordered_style_passes_one_based() {
        let input = "1. Item one\n2. Item two\n3. Item three\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Incrementing 1/2/3 should pass");
    }

    #[test]
    fn test_ordered_style_passes_zero_based() {
        let input = "0. Item zero\n1. Item one\n2. Item two\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Incrementing 0/1/2 should pass");
    }

    #[test]
    fn test_ordered_style_violates_all_ones() {
        let input = "1. Item one\n1. Item two\n1. Item three\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len(), "Should violate items 2 and 3");
        assert!(violations[0].message().contains("Expected: 2; Actual: 1"));
        assert!(violations[1].message().contains("Expected: 3; Actual: 1"));
    }

    #[test]
    fn test_ordered_style_violates_skip() {
        let input = "1. Item one\n2. Item two\n4. Item four\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len(), "Should violate skipped number");
        assert!(violations[0].message().contains("Expected: 3; Actual: 4"));
    }

    // Test "zero" style
    #[test]
    fn test_zero_style_passes() {
        let input = "0. Item zero\n0. Item zero\n0. Item zero\n";
        let config = test_config_style(OlPrefixStyle::Zero);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "All '0.' should pass");
    }

    #[test]
    fn test_zero_style_violates_ones() {
        let input = "1. Item one\n1. Item two\n1. Item three\n";
        let config = test_config_style(OlPrefixStyle::Zero);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len(), "Should violate all items");
        assert!(violations[0].message().contains("Expected: 0; Actual: 1"));
    }

    #[test]
    fn test_zero_style_violates_ordered() {
        let input = "0. Item zero\n1. Item one\n2. Item two\n";
        let config = test_config_style(OlPrefixStyle::Zero);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len(), "Should violate incrementing items");
        assert!(violations[0].message().contains("Expected: 0; Actual: 1"));
        assert!(violations[1].message().contains("Expected: 0; Actual: 2"));
    }

    // Test separate lists with document-wide consistency
    #[test]
    fn test_separate_lists_document_consistency() {
        let input = "1. First list item\n2. Second list item\n\nSome text\n\n1. New list item\n3. Should violate - expected 2\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        // First list establishes ordered style, second list should increment properly within itself
        assert_eq!(1, violations.len(), "Second list should increment properly");
        assert!(violations[0].message().contains("Expected: 2; Actual: 3"));
    }

    // Test zero-padded numbers (should work)
    #[test]
    fn test_zero_padded_ordered() {
        let input = "08. Item eight\n09. Item nine\n10. Item ten\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            0,
            violations.len(),
            "Zero-padded ordered numbers should work"
        );
    }

    // Test edge case: large numbers
    #[test]
    fn test_large_numbers() {
        let input = "100. Item hundred\n101. Item hundred-one\n102. Item hundred-two\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Large numbers should work");
    }

    // Test nested lists - each nesting level is independent
    #[test]
    fn test_nested_lists() {
        let input = "1. Outer item\n   1. Inner item\n   2. Inner item\n2. Outer item\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len(), "Nested lists should be independent");
    }

    // Test mixed ordered and unordered
    #[test]
    fn test_mixed_list_types() {
        let input = "1. Ordered item\n* Unordered item\n2. Another ordered\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let _violations = linter.analyze();
        // This depends on how tree-sitter parses this - if it creates separate lists, it should pass
        // We'll adjust based on actual tree-sitter behavior
    }

    // Test document-wide style consistency (markdownlint behavior)
    #[test]
    fn test_document_wide_style_consistency() {
        // First list establishes "ordered" style (1/2/3)
        // Subsequent lists in ordered style should start with 1 and increment
        let input = "# First section\n\n1. First item\n2. Second item\n3. Third item\n\n# Second section\n\n100. Should violate - expected 1\n102. Should violate - expected 2\n103. Should violate - expected 3\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should detect violations where second list doesn't start with 1 in ordered style
        assert_eq!(
            3,
            violations.len(),
            "Should have 3 violations for wrong start in ordered style"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 100"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 102"));
        assert!(violations[2].message().contains("Expected: 3; Actual: 103"));
    }

    #[test]
    fn test_document_wide_zero_based_style() {
        // First list establishes "zero-based ordered" style (0/1/2)
        // Subsequent lists should follow ordered style, can start with 0 or 1
        let input = "# First section\n\n0. First item\n1. Second item\n2. Third item\n\n# Second section\n\n5. Should violate - expected 1\n5. Should violate - expected 2\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(
            2,
            violations.len(),
            "Should have 2 violations for wrong start in ordered style"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 5"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 5"));
    }

    #[test]
    fn test_later_list_is_not_bound_by_earlier_style() {
        let input = "# First section\n\n1. First item\n1. Second item\n1. Third item\n\n# Second section\n\n1. Should pass\n2. Should violate - expected 1\n";
        assert!(one_or_ordered(input).is_empty());
    }

    #[test]
    fn test_fixed_style_modes_ignore_document_consistency() {
        // When using fixed styles (not one_or_ordered), each list should be independent
        let input = "# First section\n\n1. First item\n2. Second item\n\n# Second section\n\n1. Different style OK\n1. In ordered mode\n";
        let config = test_config_style(OlPrefixStyle::Ordered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // In "ordered" mode, the second list should violate because it's not incrementing
        assert_eq!(
            1,
            violations.len(),
            "Should have 1 violation in second list for not incrementing"
        );
        assert!(violations[0].message().contains("Expected: 2; Actual: 1"));
    }

    // Tests for 100% markdownlint parity
    #[test]
    fn test_markdownlint_parity_blank_separated_lists() {
        // Markdownlint treats lists separated by blank lines as separate lists
        // Each should start with 1 in ordered style
        let input = "1. First list\n2. Second item\n\n100. Second list should violate\n101. Should also violate\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should have 2 violations: 100 (expected 1) and 101 (expected 2)
        assert_eq!(
            2,
            violations.len(),
            "Should treat blank-separated lists as separate"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 100"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 101"));
    }

    #[test]
    fn test_markdownlint_parity_zero_padded_separate() {
        // Zero-padded numbers in separate list should violate
        let input = "1. First\n2. Second\n\n08. Zero-padded start\n09. Next\n10. Third\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should have 3 violations for the zero-padded list not starting with 1
        assert_eq!(
            3,
            violations.len(),
            "Zero-padded separate list should violate"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 8"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 9"));
        assert!(violations[2].message().contains("Expected: 3; Actual: 10"));
    }

    #[test]
    fn test_markdownlint_parity_single_item_style_detection() {
        // Single items in separate lists should be checked against document style
        let input = "1. First\n2. Second\n\n42. Single item should violate\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should have 1 violation - single item doesn't match established ordered style
        assert_eq!(
            1,
            violations.len(),
            "Single item should follow document style"
        );
        // Note: markdownlint shows "Style: 1/1/1" for single items, suggesting different logic
        assert!(violations[0].message().contains("Expected: 1; Actual: 42"));
    }

    #[test]
    fn test_markdownlint_parity_mixed_with_headings() {
        // Lists separated by headings are definitely separate
        let input = "# Section 1\n\n1. First\n2. Second\n\n## Section 2\n\n5. Should violate\n6. Also violate\n\n### Section 3\n\n0. Zero start\n1. Should pass\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should have 2 violations for section 2 not starting with 1
        // Section 3 with 0/1 should be OK as it establishes ordered pattern
        assert_eq!(
            2,
            violations.len(),
            "Lists in different sections should be separate"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 5"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 6"));
    }

    #[test]
    fn test_markdownlint_parity_continuous_vs_separate() {
        // This tests the core difference: what markdownlint considers one list vs separate lists
        let input = "1. Item one\n2. Item two\n3. Item three\n\n1. Should this be separate?\n2. Or continuous?\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Markdownlint would treat the second part as a separate list
        // So no violations expected (both lists follow ordered pattern correctly)
        assert_eq!(
            0,
            violations.len(),
            "Lists with proper ordered pattern should not violate"
        );
    }

    #[test]
    fn test_markdownlint_parity_text_separation() {
        // Lists separated by paragraph text should be separate
        let input = "1. First list\n2. Second item\n\nSome paragraph text here.\n\n5. Different start\n6. Should violate\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should have 2 violations for not starting with 1
        assert_eq!(
            2,
            violations.len(),
            "Text-separated lists should be independent"
        );
        assert!(violations[0].message().contains("Expected: 1; Actual: 5"));
        assert!(violations[1].message().contains("Expected: 2; Actual: 6"));
    }

    #[test]
    fn test_markdownlint_parity_one_style_detection() {
        // Test that 1/1/1 pattern is detected as "one" style and enforced
        let input = "1. All ones\n1. Pattern\n1. Here\n\n2. Should violate\n2. Different pattern\n";
        let config = test_config_style(OlPrefixStyle::OneOrOrdered);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should detect "one" style from first list, second list should violate for using 2s
        assert_eq!(2, violations.len(), "Should enforce one style globally");
        assert!(violations[0].message().contains("Expected: 1; Actual: 2"));
        assert!(violations[1].message().contains("Expected: 1; Actual: 2"));
    }
}
