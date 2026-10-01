use std::rc::Rc;

use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::{
    linter::{range_from_node_range, Context, RuleViolation},
    rules::{Rule, RuleLinter, RuleType},
};

use super::md049::{literal_ranges, marker_in_literal};

// Regex patterns to find emphasis markers with spaces
static ASTERISK_EMPHASIS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\*{1,3})(\s*)([^*\n]*?)(\s*)(\*{1,3})").expect("Invalid asterisk emphasis regex")
});

static UNDERSCORE_EMPHASIS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\_{1,3})(\s*)([^_\n]*?)(\s*)(\_{1,3})")
        .expect("Invalid underscore emphasis regex")
});

/// Whether the byte at `pos` is escaped, i.e. preceded by an odd number of backslashes. An escaped
/// marker is literal text rather than a delimiter, so `\* a \*` is not emphasis with spaces inside
/// it and markdownlint does not report it.
pub(crate) fn is_escaped(text: &str, pos: usize) -> bool {
    let bytes = text.as_bytes();
    let mut backslashes = 0;
    while backslashes < pos && bytes[pos - backslashes - 1] == b'\\' {
        backslashes += 1;
    }
    backslashes % 2 == 1
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

    fn is_in_code_context(&self, node: &Node) -> bool {
        // Check if this node is inside a code span or code block
        let mut current = Some(*node);
        while let Some(node_to_check) = current {
            match node_to_check.kind() {
                "code_span" | "fenced_code_block" | "indented_code_block" => {
                    return true;
                }
                _ => {
                    current = node_to_check.parent();
                }
            }
        }
        false
    }

    fn find_emphasis_violations_in_text(&mut self, node: &Node) {
        if self.is_in_code_context(node) {
            return;
        }

        let start_byte = node.start_byte();
        let text = {
            let source = self.context.get_document_content();
            source[start_byte..node.end_byte()].to_string()
        };

        // Literal content — code spans, link destinations, math — has no emphasis markers in it
        let literal_spans = literal_ranges(&text);

        // Check for asterisk emphasis violations
        self.check_emphasis_pattern(&text, start_byte, &ASTERISK_EMPHASIS_REGEX, &literal_spans);

        // Check for underscore emphasis violations
        self.check_emphasis_pattern(
            &text,
            start_byte,
            &UNDERSCORE_EMPHASIS_REGEX,
            &literal_spans,
        );
    }

    fn check_emphasis_pattern(
        &mut self,
        text: &str,
        text_start_byte: usize,
        regex: &Regex,
        literal_spans: &[(usize, usize)],
    ) {
        for capture in regex.captures_iter(text) {
            if let (
                Some(opening_marker),
                Some(opening_space),
                Some(_content),
                Some(closing_space),
                Some(closing_marker),
            ) = (
                capture.get(1),
                capture.get(2),
                capture.get(3),
                capture.get(4),
                capture.get(5),
            ) {
                // Only the markers have to sit outside literal content; what is between them may
                // contain code spans, links or math.
                if marker_in_literal(literal_spans, opening_marker.start(), opening_marker.end())
                    || marker_in_literal(
                        literal_spans,
                        closing_marker.start(),
                        closing_marker.end(),
                    )
                {
                    continue;
                }

                if is_escaped(text, opening_marker.start())
                    || is_escaped(text, closing_marker.start())
                {
                    continue; // An escaped marker is literal text, not a delimiter
                }

                let opening_text = opening_marker.as_str();
                let closing_text = closing_marker.as_str();

                // Only process if markers match (same type and count)
                if opening_text == closing_text {
                    // Check for space after opening marker
                    if !opening_space.as_str().is_empty() {
                        self.create_opening_space_violation(
                            opening_marker,
                            opening_space,
                            text_start_byte,
                        );
                    }

                    // Check for space before closing marker
                    if !closing_space.as_str().is_empty() {
                        self.create_closing_space_violation(
                            closing_marker,
                            closing_space,
                            text_start_byte,
                        );
                    }
                }
            }
        }
    }

    fn create_opening_space_violation(
        &mut self,
        opening_marker: regex::Match,
        opening_space: regex::Match,
        text_start_byte: usize,
    ) {
        let marker = opening_marker.as_str();
        let space = opening_space.as_str();
        let violation_start = text_start_byte + opening_marker.end();
        let violation_end = text_start_byte + opening_space.end();

        let range = crate::ast::NodeRange {
            start_byte: violation_start,
            end_byte: violation_end,
            start_point: self.byte_to_point(violation_start),
            end_point: self.byte_to_point(violation_end),
        };

        self.violations.push(RuleViolation::new(
            &MD037,
            format!("{} [Context: \"{}{}\"]", MD037.description, marker, space),
            self.context.file_path.clone(),
            range_from_node_range(&range),
        ));
    }

    fn create_closing_space_violation(
        &mut self,
        closing_marker: regex::Match,
        closing_space: regex::Match,
        text_start_byte: usize,
    ) {
        let marker = closing_marker.as_str();
        let space = closing_space.as_str();
        let violation_start = text_start_byte + closing_space.start();
        let violation_end = text_start_byte + closing_marker.end();

        let range = crate::ast::NodeRange {
            start_byte: violation_start,
            end_byte: violation_end,
            start_point: self.byte_to_point(violation_start),
            end_point: self.byte_to_point(violation_end),
        };

        self.violations.push(RuleViolation::new(
            &MD037,
            format!("{} [Context: \"{}{}\"]", MD037.description, space, marker),
            self.context.file_path.clone(),
            range_from_node_range(&range),
        ));
    }

    fn byte_to_point(&self, byte_pos: usize) -> crate::ast::Point {
        let source = self.context.get_document_content();
        let mut line = 0;
        let mut column = 0;

        for (i, ch) in source.char_indices() {
            if i >= byte_pos {
                break;
            }
            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }

        crate::ast::Point { row: line, column }
    }
}

impl RuleLinter for MD037Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            // Look for text content that might contain emphasis markers with spaces
            "text" | "inline" => {
                self.find_emphasis_violations_in_text(node);
            }
            _ => {}
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
    required_nodes: &["emphasis", "strong_emphasis"],
    new_linter: |context| Box::new(MD037Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-space-in-emphasis", RuleSeverity::Error)])
    }

    #[test]
    fn test_no_violations_valid_emphasis() {
        let config = test_config();
        let input = "This has *valid emphasis* and **valid strong** text.
Also _valid emphasis_ and __valid strong__ text.
And ***valid strong emphasis*** and ___valid strong emphasis___ text.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();
        assert_eq!(md037_violations.len(), 0);
    }

    #[test]
    fn test_violations_spaces_inside_single_asterisk() {
        let config = test_config();
        let input = "This has * invalid emphasis * with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_spaces_inside_double_asterisk() {
        let config = test_config();
        let input = "This has ** invalid strong ** with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_spaces_inside_triple_asterisk() {
        let config = test_config();
        let input = "This has *** invalid strong emphasis *** with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_spaces_inside_single_underscore() {
        let config = test_config();
        let input = "This has _ invalid emphasis _ with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_spaces_inside_double_underscore() {
        let config = test_config();
        let input = "This has __ invalid strong __ with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_spaces_inside_triple_underscore() {
        let config = test_config();
        let input = "This has ___ invalid strong emphasis ___ with spaces inside.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for opening space, one for closing space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_violations_mixed_valid_and_invalid() {
        let config = test_config();
        let input = "Mix of *valid* and * invalid * emphasis.
Also **valid** and ** invalid ** strong.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 4 violations: 2 from each invalid emphasis (opening and closing spaces)
        assert_eq!(md037_violations.len(), 4);
    }

    #[test]
    fn test_violations_one_sided_spaces() {
        let config = test_config();
        let input = "One sided *invalid * and * invalid* emphasis.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // Should find 2 violations: one for each one-sided space
        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_no_violations_in_code_blocks() {
        let config = test_config();
        let input = "Regular text with *valid* emphasis.

```markdown
This should not trigger * invalid * emphasis in code blocks.
```

More text with _valid_ emphasis.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();
        assert_eq!(md037_violations.len(), 0);
    }

    #[test]
    fn test_no_violations_in_code_spans() {
        let config = test_config();
        let input = "Regular text with `* invalid * code spans` should not trigger violations.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();
        assert_eq!(md037_violations.len(), 0);
    }

    // Both expectations below were checked against markdownlint-cli2 v0.23.3.

    #[test]
    fn test_no_violations_for_escaped_markers() {
        let config = test_config();
        let input = r"a \* not emph \* b";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // An escaped marker is literal text, so there is no emphasis to have spaces inside
        assert_eq!(md037_violations.len(), 0);
    }

    #[test]
    fn test_escaped_backslash_before_a_real_marker() {
        let config = test_config();
        // `\\` is a literal backslash, so the `*` after it is a genuine delimiter
        let input = r"a \\* real * b";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        assert_eq!(md037_violations.len(), 2);
    }

    #[test]
    fn test_no_violations_for_multiplication_in_math() {
        let config = test_config();
        let input = "$$\neCPM = 0.03 * 0.3 * 1000 = 9\n$$";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        // `*` here is multiplication inside a math block, not an emphasis marker
        assert_eq!(md037_violations.len(), 0);
    }

    #[test]
    fn test_currency_is_not_math() {
        let config = test_config();
        let input = "Cost is $5 and $10 with * real _ bad_ here";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md037_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD037")
            .collect();

        assert_eq!(md037_violations.len(), 1);
    }
}
