use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

// MD059-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD059DescriptiveLinkTextTable {
    #[serde(default)]
    pub prohibited_texts: Vec<String>,
}

impl Default for MD059DescriptiveLinkTextTable {
    fn default() -> Self {
        Self {
            prohibited_texts: vec![
                "click here".to_string(),
                "here".to_string(),
                "link".to_string(),
                "more".to_string(),
            ],
        }
    }
}

// Regular inline links: [text](url) - but NOT images ![text](url)
static RE_INLINE_LINK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^!])\[([^\]]*)\]\(([^)]+)\)").expect("Failed to compile inline link regex")
});

// Reference links: [text][ref] - but NOT images ![text][ref]
static RE_REF_LINK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^!])\[([^\]]*)\]\[([^\]]+)\]")
        .expect("Failed to compile reference link regex")
});

// Collapsed reference links: [text][] - but NOT images ![text][]
static RE_COLLAPSED_REF_LINK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^!])\[([^\]]+)\]\[\]")
        .expect("Failed to compile collapsed reference link regex")
});

static RE_NORMALIZE_PUNCTUATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\W_]+").expect("Failed to compile punctuation regex"));
static RE_NORMALIZE_WHITESPACE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s+").expect("Failed to compile whitespace regex"));

/// MD059 - Link text should be descriptive
///
/// This rule checks that link text provides meaningful description instead of generic phrases.
pub(crate) struct MD059Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    prohibited_texts: HashSet<String>,
}

impl MD059Linter {
    pub fn new(context: Rc<Context>) -> Self {
        let prohibited_texts = context
            .config
            .linters
            .settings
            .descriptive_link_text
            .prohibited_texts
            .iter()
            .map(|text| normalize_text(text))
            .collect();

        Self {
            context,
            violations: Vec::new(),
            prohibited_texts,
        }
    }
}

impl RuleLinter for MD059Linter {
    fn feed(&mut self, node: &Node) {
        // `link` is an inline kind, so it is in the tree but never fed here; the regex scan over the
        // enclosing `inline` is what finds link text.
        if node.kind() == "inline" {
            self.check_inline_for_links(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

impl MD059Linter {
    fn check_inline_for_links(&mut self, inline_node: &Node) {
        // Look for links within inline content using the text
        let link_text = {
            let document_content = self.context.document_content.borrow();
            inline_node
                .utf8_text(document_content.as_bytes())
                .unwrap_or("")
                .to_string()
        };

        // Parse the inline content for markdown links
        if !link_text.is_empty() {
            self.check_text_for_link_patterns(&link_text, inline_node);
        }
    }

    fn check_text_for_link_patterns(&mut self, text: &str, node: &Node) {
        let base = node.start_byte();
        for re in [&*RE_INLINE_LINK, &*RE_REF_LINK, &*RE_COLLAPSED_REF_LINK] {
            for caps in re.captures_iter(text) {
                if let Some(label) = caps.get(1) {
                    // markdownlint reports at the label's own line. An `inline` node spans a whole
                    // wrapped paragraph, so reporting at its range put every link in the paragraph on
                    // the paragraph's first line.
                    self.check_label_for_prohibited_text(label.as_str(), base + label.start());
                }
            }
        }
    }

    fn check_label_for_prohibited_text(&mut self, label_text: &str, label_byte: usize) {
        // Check if label text contains code or HTML - if so, skip
        if label_text.contains('`') || label_text.contains('<') {
            return;
        }

        let normalized_text = normalize_text(label_text);

        if self.prohibited_texts.contains(&normalized_text) {
            self.create_violation(label_byte, label_text);
        }
    }

    fn create_violation(&mut self, label_byte: usize, link_text: &str) {
        let end_byte = label_byte + link_text.len();
        let message = format!("Link text should be descriptive: '{link_text}'");

        self.violations.push(RuleViolation::new(
            &MD059,
            message,
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: label_byte,
                end_byte,
                start_point: self.context.point_at(label_byte),
                end_point: self.context.point_at(end_byte),
            }),
        ));
    }
}

/// Normalizes text using the same algorithm as the original markdownlint
/// Removes punctuation and extra whitespace, converts to lowercase
fn normalize_text(text: &str) -> String {
    // Replace all non-word and underscore characters with spaces
    let step1 = RE_NORMALIZE_PUNCTUATION.replace_all(text, " ");

    // Replace multiple spaces with single space
    let step2 = RE_NORMALIZE_WHITESPACE.replace_all(&step1, " ");

    // Convert to lowercase and trim
    step2.to_lowercase().trim().to_string()
}

pub const MD059: Rule = Rule {
    id: "MD059",
    alias: "descriptive-link-text",
    tags: &["accessibility", "links"],
    description: "Link text should be descriptive",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD059Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    use super::normalize_text;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("descriptive-link-text", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
            ("line-length", RuleSeverity::Off),
        ])
    }

    #[test]
    fn test_normalize_text() {
        assert_eq!("click here", normalize_text("click here"));
        assert_eq!("click here", normalize_text("Click Here"));
        assert_eq!("click here", normalize_text("click   here"));
        assert_eq!("click here", normalize_text("click_here"));
        assert_eq!("click here", normalize_text("click-here"));
        assert_eq!("click here", normalize_text("  click here  "));
        assert_eq!("click here", normalize_text("click.here!"));
    }

    #[test]
    fn test_descriptive_link_passes() {
        let input = "[Download the budget document](https://example.com/budget.pdf)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_generic_link_text_fails() {
        let input = "[click here](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
        let violation = &violations[0];
        assert_eq!("MD059", violation.rule().id);
        assert!(violation
            .message()
            .contains("Link text should be descriptive"));
        assert!(violation.message().contains("click here"));
    }

    #[test]
    fn test_prohibited_texts() {
        let test_cases = vec![
            "[here](url)",
            "[link](url)",
            "[more](url)",
            "[click here](url)",
        ];

        for input in test_cases {
            let config = test_config();
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            let violations = linter.analyze();

            assert_eq!(1, violations.len(), "Failed for input: {input}");
            let violation = &violations[0];
            assert_eq!("MD059", violation.rule().id);
        }
    }

    #[test]
    fn test_case_insensitive() {
        let input = "[CLICK HERE](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_punctuation_normalized() {
        let input = "[click-here!](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_extra_whitespace_normalized() {
        let input = "[  click   here  ](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_reference_links() {
        let input = r#"[click here][ref]

[ref]: https://example.com"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_multiple_links() {
        let input = "[good link](url1) and [click here](url2) and [another good](url3)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("click here"));
    }

    #[test]
    fn test_empty_link_text() {
        let input = "[](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Empty link text should not match prohibited texts
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_links_with_code_allowed() {
        let input = "[`click here`](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Links containing code should be allowed
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_image_links_ignored() {
        let input = "![click here](image.jpg)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Images should be ignored by this rule
        assert_eq!(0, violations.len());
    }
    /// An `inline` node spans a whole wrapped paragraph, so a violation built from its range lands
    /// on the paragraph's first line no matter where the link is. markdownlint reports at the label's
    /// own line. Every expectation is a markdownlint-cli2 0.23.3 measurement, 0-based.
    #[test]
    fn test_reports_the_line_the_link_label_is_on() {
        fn rows(input: &str) -> Vec<usize> {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD059")
                .map(|v| v.location().range.start.line)
                .collect()
        }

        assert_eq!(
            vec![1],
            rows("intro text here\nand [click here](/a) on line two\n")
        );
        assert_eq!(
            vec![0],
            rows("[here](/a) then more words\non a second line\n")
        );
        assert_eq!(vec![0, 0], rows("one [link](/x) and two [more](/y) here\n"));
        assert_eq!(vec![0, 2], rows("a [here](/x) b\n\nc [more](/y) d\n"));
        // Inside a list item, a block quote and a table cell.
        assert_eq!(vec![3], rows("- item\n\n  text\n  [click here](/a)\n"));
        assert_eq!(vec![1], rows("> quote\n> [more](/y) here\n"));
        assert_eq!(vec![2], rows("| a |\n|---|\n| [here](/x) |\n"));
        // A label holding a code span is exempt.
        assert!(rows("see [`here`](/x) ok\n").is_empty());
    }
}
