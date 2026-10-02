use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{Rule, RuleType},
};

use super::md049::literal_ranges;

// MD050-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub enum StrongStyle {
    #[serde(rename = "consistent")]
    Consistent,
    #[serde(rename = "asterisk")]
    Asterisk,
    #[serde(rename = "underscore")]
    Underscore,
}

impl Default for StrongStyle {
    fn default() -> Self {
        Self::Consistent
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD050StrongStyleTable {
    #[serde(default)]
    pub style: StrongStyle,
}

impl Default for MD050StrongStyleTable {
    fn default() -> Self {
        Self {
            style: StrongStyle::Consistent,
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
enum StrongMarkerType {
    Asterisk,
    Underscore,
}

pub(crate) struct MD050Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    first_strong_marker: Option<StrongMarkerType>,
}

impl MD050Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            first_strong_marker: None,
        }
    }

    fn is_in_code_context(&self, node: &Node) -> bool {
        // Check if this node is inside a code span or code block
        let mut current = Some(*node);
        while let Some(node_to_check) = current {
            if matches!(
                node_to_check.kind(),
                "code_span" | "fenced_code_block" | "indented_code_block"
            ) {
                return true;
            }
            current = node_to_check.parent();
        }
        false
    }

    fn find_strong_violations_in_text(&mut self, node: &Node) {
        if self.is_in_code_context(node) {
            return;
        }

        let node_start_byte = node.start_byte();
        let text = {
            let content = self.context.get_document_content();
            node.utf8_text(content.as_bytes()).unwrap_or("").to_string()
        };

        if !text.is_empty() {
            self.find_strong_patterns(&text, node_start_byte);
        }
    }

    fn find_strong_patterns(&mut self, text: &str, text_start_byte: usize) {
        let config = &self.context.config.linters.settings.strong_style;

        // Look for all strong emphasis markers - both opening and closing
        let mut i = 0;
        let chars: Vec<char> = text.chars().collect();

        // Byte offset of each char, so a marker found at char `i` can be tested against the
        // code span ranges below without re-walking the string.
        let mut char_offsets: Vec<usize> = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0;
        for c in &chars {
            char_offsets.push(acc);
            acc += c.len_utf8();
        }
        char_offsets.push(acc);

        // Markers inside literal content — a code span or a link destination — are not emphasis, so
        // they must neither set nor violate the document's consistent style.
        let literal_spans = literal_ranges(text);

        while i < chars.len() {
            if i + 1 < chars.len() {
                let current_char = chars[i];
                let next_char = chars[i + 1];

                // Check for strong emphasis markers (both ** and __)
                if (current_char == '*' && next_char == '*')
                    || (current_char == '_' && next_char == '_')
                {
                    let marker_start = char_offsets[i];
                    let marker_end = char_offsets[i + 2];
                    if literal_spans
                        .iter()
                        .any(|(s, e)| marker_start < *e && marker_end > *s)
                    {
                        i += 2;
                        continue;
                    }

                    // CommonMark forbids intraword emphasis with `_`, so the `__` in an identifier
                    // like `ANALYTICDB__29` is not a delimiter run at all.
                    if current_char == '_' {
                        let bytes = text.as_bytes();
                        let intraword = marker_start
                            .checked_sub(1)
                            .and_then(|p| bytes.get(p))
                            .is_some_and(u8::is_ascii_alphanumeric)
                            && bytes.get(marker_end).is_some_and(u8::is_ascii_alphanumeric);
                        if intraword {
                            i += 2;
                            continue;
                        }
                    }

                    // Skip if this is part of a longer sequence that would make it invalid
                    // e.g., ____ should not be detected as __ + __
                    if i + 2 < chars.len() && chars[i + 2] == current_char {
                        // This is at least a triple marker, could be *** or ___
                        if i + 3 < chars.len() && chars[i + 3] == current_char {
                            // This is a quadruple marker like ____ or ****
                            // Skip the entire sequence
                            let mut skip_count = 4;
                            while i + skip_count < chars.len()
                                && chars[i + skip_count] == current_char
                            {
                                skip_count += 1;
                            }
                            i += skip_count;
                            continue;
                        }
                        // Triple marker (*** or ___) - handle as strong emphasis
                    }

                    let marker_type = if current_char == '*' {
                        StrongMarkerType::Asterisk
                    } else {
                        StrongMarkerType::Underscore
                    };

                    // Check if we should report a violation for this marker
                    let should_report_violation = match config.style {
                        StrongStyle::Consistent => {
                            if self.first_strong_marker.is_none() {
                                self.first_strong_marker = Some(marker_type.clone());
                                false
                            } else {
                                self.first_strong_marker.as_ref() != Some(&marker_type)
                            }
                        }
                        StrongStyle::Asterisk => marker_type != StrongMarkerType::Asterisk,
                        StrongStyle::Underscore => marker_type != StrongMarkerType::Underscore,
                    };

                    if should_report_violation {
                        let expected_style = match config.style {
                            StrongStyle::Asterisk => "asterisk",
                            StrongStyle::Underscore => "underscore",
                            StrongStyle::Consistent => {
                                match self.first_strong_marker.as_ref().unwrap() {
                                    StrongMarkerType::Asterisk => "asterisk",
                                    StrongMarkerType::Underscore => "underscore",
                                }
                            }
                        };

                        let actual_style = match marker_type {
                            StrongMarkerType::Asterisk => "asterisk",
                            StrongMarkerType::Underscore => "underscore",
                        };

                        // Calculate byte position - markdownlint reports position of the second character for double markers,
                        // and the third character for opening triple markers only
                        let is_opening_triple_marker = i + 2 < chars.len()
                            && chars[i + 2] == current_char
                            && (i == 0 || (i > 0 && chars[i - 1] != current_char));
                        let position_offset = if is_opening_triple_marker { 2 } else { 1 };
                        let char_start_byte = text_start_byte
                            + text
                                .chars()
                                .take(i + position_offset)
                                .map(|c| c.len_utf8())
                                .sum::<usize>()
                            - 1;
                        let char_end_byte = char_start_byte + current_char.len_utf8();

                        let range = crate::ast::NodeRange {
                            start_byte: char_start_byte,
                            end_byte: char_end_byte,
                            start_point: self.context.point_at(char_start_byte),
                            end_point: self.context.point_at(char_end_byte),
                        };

                        self.violations.push(RuleViolation::new(
                            &MD050,
                            format!("Expected: {expected_style}; Actual: {actual_style}"),
                            self.context.file_path.clone(),
                            range_from_node_range(&range),
                        ));
                    }

                    // Move past this marker pair
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }
}

impl RuleLinter for MD050Linter {
    fn feed(&mut self, node: &Node) {
        if matches!(node.kind(), "text" | "inline") {
            self.find_strong_violations_in_text(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD050: Rule = Rule {
    id: "MD050",
    alias: "strong-style",
    tags: &["emphasis"],
    description: "Strong style should be consistent",
    rule_type: RuleType::Token,
    required_nodes: &["strong_emphasis"],
    new_linter: |context| Box::new(MD050Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{RuleSeverity, StrongStyle};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("strong-style", RuleSeverity::Error)])
    }

    fn test_config_with_style(style: StrongStyle) -> crate::config::QuickmarkConfig {
        let mut config = test_config();
        config.linters.settings.strong_style.style = style;
        config
    }

    #[test]
    fn test_no_violations_consistent_asterisk() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has **strong text** and **another strong**.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_no_violations_consistent_underscore() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has __strong text__ and __another strong__.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_violations_inconsistent_mixed() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has **strong text** and __inconsistent strong__.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find 2 violations for the inconsistent underscore strong (opening and closing)
        assert_eq!(md050_violations.len(), 2);
    }

    #[test]
    fn test_no_violations_asterisk_style() {
        let config = test_config_with_style(StrongStyle::Asterisk);
        let input = "This has **strong text** and **another strong**.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_violations_asterisk_style_with_underscore() {
        let config = test_config_with_style(StrongStyle::Asterisk);
        let input = "This has **strong text** and __invalid strong__.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find 2 violations for the underscore strong when asterisk is required (opening and closing)
        assert_eq!(md050_violations.len(), 2);
    }

    #[test]
    fn test_no_violations_underscore_style() {
        let config = test_config_with_style(StrongStyle::Underscore);
        let input = "This has __strong text__ and __another strong__.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_violations_underscore_style_with_asterisk() {
        let config = test_config_with_style(StrongStyle::Underscore);
        let input = "This has __strong text__ and **invalid strong**.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find 2 violations for the asterisk strong when underscore is required (opening and closing)
        assert_eq!(md050_violations.len(), 2);
    }

    #[test]
    fn test_mixed_emphasis_and_strong() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has *emphasis* and **strong** and __inconsistent strong__.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find 2 violations for the inconsistent strong (opening and closing, emphasis should not be considered)
        assert_eq!(md050_violations.len(), 2);
    }

    #[test]
    fn test_strong_emphasis_combination() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has ***strong emphasis*** and ***another***.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find no violations as both use asterisk consistently
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_strong_emphasis_inconsistent() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "This has ***strong emphasis*** and ___inconsistent___. ";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // Should find 2 violations for the inconsistent strong emphasis (opening and closing)
        assert_eq!(md050_violations.len(), 2);
    }

    #[test]
    fn test_code_span_does_not_set_consistent_style() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "# `a__b`

**bold**";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        // The only real strong emphasis is `**bold**`, so it defines the style and nothing violates
        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_code_span_marker_is_not_a_violation() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "**bold** and `__literal__`";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_longer_backtick_runs_are_code_spans() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "x `a__b` y ``c__d`` z and **bold**";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_link_destination_is_not_strong() {
        let config = test_config_with_style(StrongStyle::Consistent);
        // `__biz` is part of the URL, so `**bold**` is the only strong and sets the style
        let input = "see [doc](http://example.com/s?__biz=ABC&y=1) and **bold**";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_autolink_destination_is_not_strong() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "<http://example.com/__a__> and **bold**";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        assert_eq!(md050_violations.len(), 0);
    }

    #[test]
    fn test_intraword_underscores_are_not_strong() {
        let config = test_config_with_style(StrongStyle::Consistent);
        let input = "shard ANALYTICDB__29 and ckp_batch_ANALYTICDB__29_x.tar with **bold**";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md050_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD050")
            .collect();

        assert_eq!(md050_violations.len(), 0);
    }
}
