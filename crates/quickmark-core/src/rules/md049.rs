use serde::Deserialize;
use std::rc::Rc;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::Node;

use crate::{
    linter::{range_from_tree_sitter, Context, RuleViolation},
    rules::{Rule, RuleLinter, RuleType},
};

// MD049-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub enum EmphasisStyle {
    #[serde(rename = "consistent")]
    Consistent,
    #[serde(rename = "asterisk")]
    Asterisk,
    #[serde(rename = "underscore")]
    Underscore,
}

impl Default for EmphasisStyle {
    fn default() -> Self {
        Self::Consistent
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD049EmphasisStyleTable {
    #[serde(default)]
    pub style: EmphasisStyle,
}

impl Default for MD049EmphasisStyleTable {
    fn default() -> Self {
        Self {
            style: EmphasisStyle::Consistent,
        }
    }
}

// Regex patterns to find emphasis
static ASTERISK_EMPHASIS_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\*([^*\n]+?)\*").expect("Invalid asterisk emphasis regex"));

static UNDERSCORE_EMPHASIS_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"_([^_\n]+?)_").expect("Invalid underscore emphasis regex"));

// Regex to find code spans (to exclude from emphasis checking)
pub(crate) static CODE_SPAN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"`[^`\n]*`").expect("Invalid code span regex"));

#[derive(Debug, Clone, Copy, PartialEq)]
enum DetectedEmphasisStyle {
    Asterisk,
    Underscore,
}

pub(crate) struct MD049Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    document_style: Option<DetectedEmphasisStyle>,
}

impl MD049Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            document_style: None,
        }
    }

    fn get_configured_style(&self) -> EmphasisStyle {
        self.context
            .config
            .linters
            .settings
            .emphasis_style
            .style
            .clone()
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

    fn is_intraword_emphasis(
        &self,
        _text: &str,
        start_offset: usize,
        emphasis_start: usize,
        emphasis_end: usize,
    ) -> bool {
        let emphasis_global_start = start_offset + emphasis_start;
        let emphasis_global_end = start_offset + emphasis_end;
        let source = self.context.get_document_content();

        // Check character before emphasis start
        let before_is_word_char = if emphasis_global_start > 0 {
            if let Some(ch) = source.chars().nth(emphasis_global_start - 1) {
                ch.is_alphanumeric() || ch == '_'
            } else {
                false
            }
        } else {
            false
        };

        // Check character after emphasis end
        let after_is_word_char = if emphasis_global_end < source.len() {
            if let Some(ch) = source.chars().nth(emphasis_global_end) {
                ch.is_alphanumeric() || ch == '_'
            } else {
                false
            }
        } else {
            false
        };

        before_is_word_char || after_is_word_char
    }

    fn process_emphasis_matches(
        &mut self,
        text: &str,
        start_offset: usize,
        matches: &[(usize, usize, DetectedEmphasisStyle)],
    ) {
        // Find code span ranges to exclude
        let code_span_ranges: Vec<(usize, usize)> = CODE_SPAN_REGEX
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect();

        for &(match_start, match_end, style) in matches {

            // Check if this match overlaps with any code span
            let in_code_span = code_span_ranges
                .iter()
                .any(|(code_start, code_end)| match_start < *code_end && match_end > *code_start);

            if in_code_span {
                continue; // Skip this match as it's inside a code span
            }

            // Check if this is intraword emphasis
            if self.is_intraword_emphasis(text, start_offset, match_start, match_end) {
                // Intraword emphasis is always allowed regardless of configured style
                continue;
            }

            let configured_style = self.get_configured_style();
            let should_report_violation = match configured_style {
                EmphasisStyle::Asterisk => style != DetectedEmphasisStyle::Asterisk,
                EmphasisStyle::Underscore => style != DetectedEmphasisStyle::Underscore,
                EmphasisStyle::Consistent => {
                    if let Some(doc_style) = self.document_style {
                        style != doc_style
                    } else {
                        // First emphasis sets the document style
                        self.document_style = Some(style);
                        false // No violation for the first emphasis
                    }
                }
            };

            if should_report_violation {
                let expected_style = match configured_style {
                    EmphasisStyle::Asterisk => "asterisk",
                    EmphasisStyle::Underscore => "underscore",
                    EmphasisStyle::Consistent => match self.document_style {
                        Some(DetectedEmphasisStyle::Asterisk) => "asterisk",
                        Some(DetectedEmphasisStyle::Underscore) => "underscore",
                        None => "consistent", // This shouldn't happen, but fallback
                    },
                };

                let actual_style = match style {
                    DetectedEmphasisStyle::Asterisk => "asterisk",
                    DetectedEmphasisStyle::Underscore => "underscore",
                };

                // markdownlint reports the opening and the closing marker separately, since each is
                // its own edit; match that so the violation counts agree.
                let message = format!("Expected: {expected_style}; Actual: {actual_style}");
                for marker_start in [match_start, match_end - 1] {
                    let global_start = start_offset + marker_start;
                    let global_end = global_start + 1;

                    let range = tree_sitter::Range {
                        start_byte: global_start,
                        end_byte: global_end,
                        start_point: self.byte_to_point(global_start),
                        end_point: self.byte_to_point(global_end),
                    };

                    self.violations.push(RuleViolation::new(
                        &MD049,
                        message.clone(),
                        self.context.file_path.clone(),
                        range_from_tree_sitter(&range),
                    ));
                }
            }
        }
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

        // eprintln!("DEBUG MD049: Processing text: '{}'", text);

        // Both marker kinds have to be considered in a single document-order pass: whichever
        // emphasis comes first defines the "consistent" style. Running the asterisk regex to
        // completion first would let a later `*x*` set the style over an earlier `_y_`.
        let mut matches: Vec<(usize, usize, DetectedEmphasisStyle)> = ASTERISK_EMPHASIS_REGEX
            .find_iter(&text)
            .filter(|m| is_plausible_emphasis(&text, m.start(), m.end(), b'*'))
            .map(|m| (m.start(), m.end(), DetectedEmphasisStyle::Asterisk))
            .chain(
                UNDERSCORE_EMPHASIS_REGEX
                    .find_iter(&text)
                    .filter(|m| is_plausible_emphasis(&text, m.start(), m.end(), b'_'))
                    .map(|m| (m.start(), m.end(), DetectedEmphasisStyle::Underscore)),
            )
            .collect();
        matches.sort_by_key(|(start, _, _)| *start);

        self.process_emphasis_matches(&text, start_byte, &matches);
    }

    fn byte_to_point(&self, byte_pos: usize) -> tree_sitter::Point {
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

        tree_sitter::Point { row: line, column }
    }
}

/// Whether a regex hit is plausibly real emphasis rather than an artifact of scanning raw text.
/// micromark settles this with the full flanking rules; quickmark never parses the inline tree, so
/// these are the two cheap conditions that remove the common false positives — a `**strong**` run,
/// which is MD050's business, and markers used as ordinary punctuation, as in `a * b * c`.
fn is_plausible_emphasis(text: &str, start: usize, end: usize, marker: u8) -> bool {
    let bytes = text.as_bytes();

    // Not one emphasis inside a longer run of the same marker.
    if start > 0 && bytes[start - 1] == marker {
        return false;
    }
    if bytes.get(end) == Some(&marker) {
        return false;
    }

    // An opener must be followed by content and a closer preceded by it, so whitespace either side
    // of the enclosed text means these were never delimiter runs.
    !matches!(bytes.get(start + 1), Some(b' ') | Some(b'\t'))
        && !matches!(bytes.get(end - 2), Some(b' ') | Some(b'\t'))
}

impl RuleLinter for MD049Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            // Look for text content that might contain emphasis
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

pub const MD049: Rule = Rule {
    id: "MD049",
    alias: "emphasis-style",
    tags: &["emphasis"],
    description: "Emphasis style",
    rule_type: RuleType::Token,
    required_nodes: &["emphasis"],
    new_linter: |context| Box::new(MD049Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("emphasis-style", RuleSeverity::Error)])
    }

    #[test]
    fn test_consistent_style_asterisk_should_pass() {
        let config = test_config();
        let input = "This has *valid* emphasis and *more* emphasis.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md049_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .collect();
        assert_eq!(md049_violations.len(), 0);
    }

    #[test]
    fn test_consistent_style_underscore_should_pass() {
        let config = test_config();
        let input = "This has _valid_ emphasis and _more_ emphasis.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md049_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .collect();
        assert_eq!(md049_violations.len(), 0);
    }

    #[test]
    fn test_mixed_styles_should_fail() {
        let config = test_config();
        let input = "This has *asterisk* emphasis and _underscore_ emphasis.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md049_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .collect();
        // Should find violations for the inconsistent emphasis (underscore when asterisk was first)
        assert!(!md049_violations.is_empty());
    }

    #[test]
    fn test_intraword_emphasis_should_be_preserved() {
        let config = test_config();
        let input = "This has apple*banana*cherry and normal *emphasis* as well.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md049_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .collect();
        // Intraword emphasis should not be checked for style consistency
        assert_eq!(md049_violations.len(), 0);
    }

    #[test]
    fn test_nested_emphasis_mixed_styles() {
        let config = test_config();
        let input = "This paragraph *nests both _kinds_ of emphasis* marker.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md049_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .collect();
        // Should find violations for the inconsistent nested emphasis
        assert!(!md049_violations.is_empty());
    }

    fn md049_messages(input: &str) -> Vec<String> {
        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter
            .analyze()
            .iter()
            .filter(|v| v.rule().id == "MD049")
            .map(|v| v.message().to_string())
            .collect()
    }

    // Every expectation below was checked against markdownlint-cli2 v0.23.3 (markdownlint v0.41.1).

    #[test]
    fn test_reports_both_markers_of_an_offending_emphasis() {
        let messages = md049_messages("This has *emphasis* and _inconsistent_.");
        assert_eq!(
            vec![
                "Expected: asterisk; Actual: underscore",
                "Expected: asterisk; Actual: underscore"
            ],
            messages.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_consistent_style_follows_document_order() {
        // The first emphasis in the document sets the style, so `_first_` wins over the later
        // `*second*` even though the asterisk regex is the one that runs first.
        let messages = md049_messages("_first_ then *second*");
        assert_eq!(2, messages.len());
        assert!(
            messages
                .iter()
                .all(|m| m == "Expected: underscore; Actual: asterisk"),
            "unexpected messages: {messages:?}"
        );
    }

    #[test]
    fn test_strong_is_not_counted_as_emphasis() {
        // `**strong**` is MD050's business, so `_emph_` is the only emphasis and sets the style.
        assert!(md049_messages("**strong** and _emph_").is_empty());
    }

    #[test]
    fn test_spaced_markers_are_not_emphasis() {
        // Neither `*` is a delimiter run here, so `_real_` sets the style unopposed.
        assert!(md049_messages("a * b * c and _real_").is_empty());
    }
}
