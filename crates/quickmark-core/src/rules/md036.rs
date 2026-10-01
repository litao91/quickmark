use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::md037::is_escaped;
use super::md049::{CODE_SPAN_REGEX, MATH_REGEX};
use super::{Rule, RuleType};

// Inline content that micromark tokenises as something other than `data`, which is the only child
// markdownlint tolerates inside a reported emphasis. Each alternative is one token type: character
// escape, inline HTML tag, angle-bracket autolink, GFM autolink literal, GFM email literal, and
// character reference. A bare `<`, `&` or `]` stays literal text, so none of those characters
// appears here on its own.
static NON_DATA_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"\\[!-/:-@\[-_`{-~]",
        r"|</?[A-Za-z][A-Za-z0-9-]*(?:[\s/][^>\n]*)?>",
        r"|<(?:[A-Za-z][A-Za-z0-9+.-]*:[^<>\n]*|[^<>\s@]+@[^<>\s@]+\.[^<>\s]+)>",
        r"|(?:www\.|[A-Za-z][A-Za-z0-9+.-]*://)[^<>\s]",
        r"|[^<>\s@]+@[^<>\s@]+\.[A-Za-z]{2,}",
        r"|&(?:#[0-9]+|#[xX][0-9a-fA-F]+|[A-Za-z][A-Za-z0-9]{1,31});",
    ))
    .expect("Invalid MD036 non-data regex")
});

/// A run of `*` or `_` that CommonMark may use to open or close emphasis.
struct DelimiterRun {
    marker: u8,
    len: usize,
    can_open: bool,
    can_close: bool,
}

/// The delimiter runs in `text`, approximating CommonMark's flanking rules: a run opens when
/// non-whitespace follows it and closes when non-whitespace precedes it. A `_` between two
/// alphanumeric characters is not a delimiter at all, which is why `snake_case_name` is plain text
/// while `_name_` is not.
fn delimiter_runs(text: &str) -> Vec<DelimiterRun> {
    let bytes = text.as_bytes();
    let mut runs = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let marker = bytes[index];
        if (marker != b'*' && marker != b'_') || is_escaped(text, index) {
            index += 1;
            continue;
        }
        let end = index + bytes[index..].iter().take_while(|&&b| b == marker).count();
        let before = index.checked_sub(1).map(|previous| bytes[previous]);
        let after = bytes.get(end).copied();
        let intraword = marker == b'_'
            && before.is_some_and(|byte| byte.is_ascii_alphanumeric())
            && after.is_some_and(|byte| byte.is_ascii_alphanumeric());
        if !intraword {
            runs.push(DelimiterRun {
                marker,
                len: end - index,
                can_open: after.is_some_and(|byte| !byte.is_ascii_whitespace()),
                can_close: before.is_some_and(|byte| !byte.is_ascii_whitespace()),
            });
        }
        index = end;
    }
    runs
}

/// Whether CommonMark's "rule of 3" keeps two runs apart: when either run can both open and close,
/// their lengths must not sum to a multiple of three unless both are themselves multiples of three.
/// This is what makes `**a*b**` one strong emphasis rather than a nested pair.
fn rule_of_three_blocks(opener_can_close: bool, opener_len: usize, closer: &DelimiterRun) -> bool {
    (opener_can_close || closer.can_open)
        && (opener_len + closer.len).is_multiple_of(3)
        && !(opener_len.is_multiple_of(3) && closer.len.is_multiple_of(3))
}

/// Whether `text` holds a matched emphasis pair. A marker that never finds a partner stays literal
/// data, so counting markers is not enough — `**a*b**` is plain text but `**a*b*c**` is not.
fn has_emphasis_pair(text: &str) -> bool {
    let runs = delimiter_runs(text);
    runs.iter().enumerate().any(|(opener_index, opener)| {
        opener.can_open
            && runs[opener_index + 1..].iter().any(|closer| {
                closer.marker == opener.marker
                    && closer.can_close
                    && !rule_of_three_blocks(opener.can_close, opener.len, closer)
            })
    })
}

/// Whether the leading delimiter run of an emphasis is claimed by a run inside its own content,
/// which leaves trailing text in the paragraph. In `**a**b**` the inner `**` closes the emphasis
/// after `a`, so the paragraph is a strong plus the literal `b**` — two children, not one.
fn opener_claimed_by_inner(inner: &str, outer_len: usize, marker: u8) -> bool {
    delimiter_runs(inner)
        .iter()
        // The leading run sits at the start of the paragraph, so it can never close.
        .any(|run| {
            run.marker == marker && run.can_close && !rule_of_three_blocks(false, outer_len, run)
        })
}

/// The content of `text` when the whole inline is a single emphasis span, otherwise `None`.
///
/// `***x***` nests an emphasis inside a strong, so neither token holds plain data on its own and
/// the paragraph is left alone. Leading indentation is consumed at block level and does not matter,
/// but whitespace *after* the closing delimiter does — the caller checks for it in the document.
fn sole_emphasis_content(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let bytes = text.as_bytes();
    let marker = *bytes.first()?;
    if marker != b'*' && marker != b'_' {
        return None;
    }
    let run_len = bytes.iter().take_while(|&&byte| byte == marker).count();
    if !matches!(run_len, 1 | 2) || bytes.len() <= 2 * run_len {
        return None;
    }
    let closing = &bytes[bytes.len() - run_len..];
    if closing.iter().any(|&byte| byte != marker) {
        return None;
    }
    // Markers adjacent to the content belong to the delimiters, so the run lengths would be wrong.
    if bytes[run_len] == marker || bytes[bytes.len() - run_len - 1] == marker {
        return None;
    }
    let inner = &text[run_len..bytes.len() - run_len];
    (!opener_claimed_by_inner(inner, run_len, marker)).then_some(inner)
}

/// Whether the emphasized content carries inline markup of its own. markdownlint reports an
/// emphasis only when its text child is a single `data` token, so `**\`code\`**`, `**$x$**`,
/// `**a [b] c**` and `**a _b_ c**` are all left alone. An unmatched marker is literal and does not
/// count, which is why the emphasis scan is paired rather than a search for `*`.
fn contains_inline_markup(text: &str) -> bool {
    text.contains('\n')
        || text.contains('[')
        || NON_DATA_REGEX.is_match(text)
        || CODE_SPAN_REGEX.is_match(text)
        || MATH_REGEX.is_match(text)
        || has_emphasis_pair(text)
}

/// markdownlint only considers a paragraph whose micromark parent is the document's `content`
/// chunk, which excludes anything nested in a list item, block quote, table cell or HTML block.
/// tree-sitter wraps document-level blocks in `section` nodes, so those are the only other
/// ancestors a candidate paragraph may have.
fn is_document_level(node: &Node) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if !matches!(ancestor.kind(), "section" | "document") {
            return false;
        }
        current = ancestor.parent();
    }
    true
}

// MD036-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD036EmphasisAsHeadingTable {
    #[serde(default)]
    pub punctuation: String,
}

impl Default for MD036EmphasisAsHeadingTable {
    fn default() -> Self {
        Self {
            punctuation: ".,;:!?。，；：！？".to_string(),
        }
    }
}

pub(crate) struct MD036Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD036Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    fn check_paragraph_for_emphasis_heading(&mut self, paragraph_node: &Node) {
        if !is_document_level(paragraph_node) {
            return;
        }

        // A paragraph holds a single inline node covering all of its content.
        let Some(inline_node) = paragraph_node.named_child(0) else {
            return;
        };
        if inline_node.kind() != "inline" {
            return;
        }

        let source = self.context.get_document_content();
        let inline_text = &source[inline_node.start_byte()..inline_node.end_byte()];
        let Some(inner_text) = sole_emphasis_content(inline_text) else {
            return;
        };
        // Whitespace trailing the closing delimiter is a token of its own and counts as a second
        // meaningful child, so `**x** ` reads as prose. Leading indentation does not: it is
        // consumed at block level. The inline node ends before the trailing whitespace, so it has
        // to be looked for in the document rather than in `inline_text`.
        if source[inline_node.end_byte()..].starts_with([' ', '\t']) {
            return;
        }
        if inner_text.trim().is_empty() || contains_inline_markup(inner_text) {
            return;
        }

        let punctuation_chars = &self
            .context
            .config
            .linters
            .settings
            .emphasis_as_heading
            .punctuation;
        if inner_text
            .chars()
            .next_back()
            .is_some_and(|last_char| punctuation_chars.contains(last_char))
        {
            return; // A sentence ending in punctuation reads as prose, not as a heading
        }

        let start = inline_node.start_position();
        let end = inline_node.end_position();
        let range = crate::ast::NodeRange {
            start_byte: 0, // Not used by range_from_node_range
            end_byte: 0,   // Not used by range_from_node_range
            start_point: crate::ast::Point {
                row: start.row,
                column: start.column,
            },
            end_point: crate::ast::Point {
                row: end.row,
                column: end.column,
            },
        };

        self.violations.push(RuleViolation::new(
            &MD036,
            format!("Emphasis used instead of heading: '{}'", inner_text.trim()),
            self.context.file_path.clone(),
            range_from_node_range(&range),
        ));
    }
}

impl RuleLinter for MD036Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "paragraph" => self.check_paragraph_for_emphasis_heading(node),
            _ => {
                // Ignore other nodes
            }
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD036: Rule = Rule {
    id: "MD036",
    alias: "no-emphasis-as-heading",
    tags: &["headings", "emphasis"],
    description: "Emphasis used instead of a heading",
    rule_type: RuleType::Token,
    required_nodes: &["paragraph"],
    new_linter: |context| Box::new(MD036Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD036EmphasisAsHeadingTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    fn test_config(punctuation: &str) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("no-emphasis-as-heading", RuleSeverity::Error)],
            LintersSettingsTable {
                emphasis_as_heading: MD036EmphasisAsHeadingTable {
                    punctuation: punctuation.to_string(),
                },
                ..Default::default()
            },
        )
    }

    fn test_default_config() -> crate::config::QuickmarkConfig {
        test_config(".,;:!?。，；：！？")
    }

    #[test]
    fn test_emphasis_as_heading_violation() {
        let config = test_default_config();
        let input = "**Section 1**\n\nSome content here.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message().contains("Section 1"));
    }

    #[test]
    fn test_italic_emphasis_as_heading_violation() {
        let config = test_default_config();
        let input = "*Section 1*\n\nSome content here.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message().contains("Section 1"));
    }

    #[test]
    fn test_valid_emphasis_in_paragraph() {
        let config = test_default_config();
        let input = "This is a normal paragraph with **some emphasis** in it.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_emphasis_with_punctuation_allowed() {
        let config = test_default_config();
        let input = "**This ends with punctuation.**\n\nSome content.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_multiline_emphasis_allowed() {
        let config = test_default_config();
        let input = "**This is an entire paragraph that has been emphasized\nand spans multiple lines**\n\nContent.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_custom_punctuation() {
        let config = test_config(".,;:");
        let input = "**This heading has exclamation!**\n\nContent.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1); // '!' not in custom punctuation
    }

    #[test]
    fn test_custom_punctuation_with_allowed() {
        let config = test_config(".,;:");
        let input = "**This heading has period.**\n\nContent.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_mixed_emphasis_and_normal_text() {
        let config = test_default_config();
        let input = "**Violation here**\n\nThis is a normal paragraph\n**that just happens to have emphasized text in**\neven though the emphasized text is on its own line.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1); // Only the first one should be flagged
    }

    #[test]
    fn test_emphasis_with_link() {
        let config = test_default_config();
        let input = "**[This is a link](https://example.com)**\n\nContent.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0); // Links should be allowed
    }

    #[test]
    fn test_full_width_punctuation() {
        let config = test_default_config();
        let input = "**Section with full-width punctuation。**\n\nContent.";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    /// Every expectation below was measured against markdownlint-cli2 v0.23.3, whose MD036 reports
    /// only a paragraph whose single meaningful child is an emphasis token holding one `data` token.
    fn count(input: &str) -> usize {
        let config = test_default_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter.analyze().len()
    }

    #[test]
    fn test_emphasis_wrapping_code_span_allowed() {
        assert_eq!(count("**`Lifecycle#start`**\n\nContent."), 0);
        assert_eq!(count("**a `b` c**\n\nContent."), 0);
    }

    #[test]
    fn test_emphasis_wrapping_math_allowed() {
        assert_eq!(count("**$x^2$**\n\nContent."), 0);
        assert_eq!(count("**a $b$ c**\n\nContent."), 0);
        // An unpaired `$$` is literal text, so the content really is plain.
        assert_eq!(count("**a $$ b**\n\nContent."), 1);
    }

    #[test]
    fn test_emphasis_in_block_quote_allowed() {
        assert_eq!(count("> **Cut**\n\nContent."), 0);
        assert_eq!(count("> _License: TBD_\n\nContent."), 0);
    }

    #[test]
    fn test_two_emphasis_spans_in_one_paragraph_allowed() {
        assert_eq!(count("**Lexical analysis** or **scanning**\n\nContent."), 0);
        // The inner `**` closes the leading run, leaving `b**` as a second child.
        assert_eq!(count("**a**b**\n\nContent."), 0);
    }

    #[test]
    fn test_nested_emphasis_allowed() {
        assert_eq!(count("**an _lvalue_ or an _rvalue_**\n\nContent."), 0);
        assert_eq!(count("**a*b*c**\n\nContent."), 0);
        assert_eq!(count("**_a_**\n\nContent."), 0);
        assert_eq!(count("***bold italic***\n\nContent."), 0);
    }

    #[test]
    fn test_unpaired_markers_are_plain_text() {
        // A marker that never finds a partner stays `data`, so the emphasis still qualifies.
        assert_eq!(count("**a*b**\n\nContent."), 1);
        assert_eq!(count("**a_b**\n\nContent."), 1);
        assert_eq!(count("**snake_case_name**\n\nContent."), 1);
        assert_eq!(count("**a]b**\n\nContent."), 1);
        assert_eq!(count("**a<b**\n\nContent."), 1);
        assert_eq!(count("**AT&T**\n\nContent."), 1);
        assert_eq!(count("**a ~~b~~ c**\n\nContent."), 1);
    }

    #[test]
    fn test_markup_characters_that_are_not_plain_text() {
        assert_eq!(count("**a\\_b**\n\nContent."), 0); // character escape
        assert_eq!(count("**a&nbsp;b**\n\nContent."), 0); // character reference
        assert_eq!(count("**<a>**\n\nContent."), 0); // inline HTML
        assert_eq!(count("**see www.example.com now**\n\nContent."), 0); // GFM autolink literal
        assert_eq!(count("**mail foo@bar.com ok**\n\nContent."), 0); // GFM email literal
        assert_eq!(count("**a[b**\n\nContent."), 0); // label start
                                                     // A reference without its semicolon is not recognised, so this one stays plain.
        assert_eq!(count("**&amp**\n\nContent."), 1);
    }

    #[test]
    fn test_trailing_whitespace_allowed() {
        // Trailing whitespace is a token of its own and makes the paragraph two children.
        assert_eq!(count("**x** \n\nContent."), 0);
        assert_eq!(count("**x**\t\n\nContent."), 0);
        // Leading indentation is consumed at block level and changes nothing.
        assert_eq!(count("  **x**\n\nContent."), 1);
    }
}
