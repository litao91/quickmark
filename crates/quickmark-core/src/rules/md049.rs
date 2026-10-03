use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleViolation},
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Marker {
    Asterisk,
    Underscore,
}

impl Marker {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Marker::Asterisk => "asterisk",
            Marker::Underscore => "underscore",
        }
    }
}

pub(crate) struct MD049Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    document_style: Option<Marker>,
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

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch. Pre-order matches markdownlint's token order, which is what makes
    /// `style = "consistent"` mean "whatever came first in the document".
    fn walk(&mut self, root: Node) {
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "emphasis" {
                self.check(node);
            }
            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
                    return;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                cursor.goto_parent();
                depth -= 1;
            }
        }
    }

    fn check(&mut self, node: Node) {
        let (start, end) = (node.start_byte(), node.end_byte());
        let Some(marker) = self.marker_at(start) else {
            return;
        };

        let expected = match self.get_configured_style() {
            EmphasisStyle::Asterisk => Marker::Asterisk,
            EmphasisStyle::Underscore => Marker::Underscore,
            // The first emphasis in the document sets the style and is never itself a violation.
            EmphasisStyle::Consistent => *self.document_style.get_or_insert(marker),
        };
        if expected == marker {
            return;
        }

        // markdownlint only exempts intraword emphasis when the expected style is underscore, and
        // only on its own `/^\w$/` — ASCII, so a CJK character beside the marker does not count.
        if expected == Marker::Underscore && self.is_intraword(start, end) {
            return;
        }

        // markdownlint reports the opening and the closing delimiter separately, since each is its
        // own edit; match that so the violation counts agree.
        let message = format!(
            "{} [Expected: {}; Actual: {}]",
            MD049.description,
            expected.name(),
            marker.name()
        );
        let start_point = node.start_position();
        let end_point = node.end_position();
        for (marker_byte, from, to) in [
            (
                start,
                start_point,
                crate::ast::Point::new(start_point.row, start_point.column + 1),
            ),
            (
                end - 1,
                crate::ast::Point::new(end_point.row, end_point.column - 1),
                end_point,
            ),
        ] {
            let range = crate::ast::NodeRange {
                start_byte: marker_byte,
                end_byte: marker_byte + 1,
                start_point: from,
                end_point: to,
            };
            self.violations.push(RuleViolation::new(
                &MD049,
                message.clone(),
                self.context.file_path.clone(),
                range_from_node_range(&range),
            ));
        }
    }

    /// Which marker an `emphasis` node was written with. The node's own source starts at its opening
    /// delimiter, so the first byte settles it.
    fn marker_at(&self, byte: usize) -> Option<Marker> {
        match self.context.get_document_content().as_bytes().get(byte) {
            Some(b'*') => Some(Marker::Asterisk),
            Some(b'_') => Some(Marker::Underscore),
            _ => None,
        }
    }

    /// Whether a word character sits immediately outside either delimiter, making the emphasis
    /// intraword. Both offsets are character boundaries because they came from a node's own span.
    fn is_intraword(&self, start: usize, end: usize) -> bool {
        let source = self.context.get_document_content();
        let word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        word(source[..start].chars().next_back()) || word(source[end..].chars().next())
    }
}

impl RuleLinter for MD049Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.walk(*node);
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
    required_nodes: &["inline"],
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
                "Emphasis style [Expected: asterisk; Actual: underscore]",
                "Emphasis style [Expected: asterisk; Actual: underscore]"
            ],
            messages.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_emphasis_inside_a_table_cell_counts() {
        // comrak's table cells are autocompleted to the header width and their spans include the
        // surrounding padding, so the facade synthesizes cells from the raw lines and grafts
        // comrak's inline subtree onto them. Without the graft a cell is a leaf and every rule that
        // scans `inline` is blind inside tables.
        let messages = md049_messages("_a_\n\n| *b* | c |\n|---|---|\n");
        assert_eq!(
            vec![
                "Emphasis style [Expected: underscore; Actual: asterisk]",
                "Emphasis style [Expected: underscore; Actual: asterisk]"
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
                .all(|m| m == "Emphasis style [Expected: underscore; Actual: asterisk]"),
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

    #[test]
    fn test_link_destination_is_not_emphasis() {
        // `_a_` is part of the URL, so `*emph*` is the only emphasis and sets the style.
        assert!(md049_messages("[doc](http://example.com/s?_a_b_c=1) and *emph*").is_empty());
    }

    #[test]
    fn test_longer_backtick_runs_are_code_spans() {
        assert!(md049_messages("x `a_b_c` y ``d_e_f`` z and *emph*").is_empty());
    }

    #[test]
    fn test_inline_math_is_not_emphasis() {
        // `$a_b_c$` is one math node, so nothing inside it is a delimiter and `*emph*` sets the
        // style.
        assert!(md049_messages("text $a_b_c$ more *emph*").is_empty());
    }

    #[test]
    fn test_display_math_block_is_not_emphasis() {
        let messages = md049_messages("text\n\n$$\na_b_c\n$$\n\n*emph* and _other_");
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
        assert!(messages
            .iter()
            .all(|m| m == "Emphasis style [Expected: asterisk; Actual: underscore]"));
    }

    #[test]
    fn test_currency_is_not_math() {
        // A `$` amount is not a math delimiter, so `_first_` still sets the style
        let messages = md049_messages("price $5 and $10 then _first_ and *second*");
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
        assert!(messages
            .iter()
            .all(|m| m == "Emphasis style [Expected: underscore; Actual: asterisk]"));
    }

    #[test]
    fn test_multibyte_characters_do_not_shift_the_intraword_check() {
        // The em dash is three bytes but one character. Indexing characters with a byte offset put
        // the intraword test on the wrong character and discarded both emphases entirely.
        let messages = md049_messages("plain \u{2014} text _a_ and *b*");
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
        assert!(messages
            .iter()
            .all(|m| m == "Emphasis style [Expected: underscore; Actual: asterisk]"));
    }

    #[test]
    fn test_emphasis_spanning_a_wrapped_line() {
        let messages = md049_messages("Some *emphasis\nspanning lines* and _other_");
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
        assert!(messages
            .iter()
            .all(|m| m == "Emphasis style [Expected: asterisk; Actual: underscore]"));
    }

    #[test]
    fn test_emphasis_may_contain_a_code_span() {
        // A code span between the markers does not break the emphasis
        let messages = md049_messages(
            "_170 case(s) slower than baseline `161570`, 20 deep-dived_\n\nlater *emph* here",
        );
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
    }

    #[test]
    fn test_emphasis_may_span_a_code_span_holding_the_marker() {
        // The `*` inside the code span is literal, so the emphasis runs from the first `*` to the
        // last one and contains `text`, `code_span`, `text`. A pattern that matched `*…*` over raw
        // text paired the wrong two markers here and saw no emphasis at all.
        let messages = md049_messages("_x_\n\n*a `b_*` c*\n");
        assert_eq!(2, messages.len(), "unexpected: {messages:?}");
        assert!(
            messages
                .iter()
                .all(|m| m == "Emphasis style [Expected: underscore; Actual: asterisk]"),
            "unexpected: {messages:?}"
        );
    }

    /// markdownlint parses with micromark's `math()` at its defaults, so `$…$` is a math token and
    /// an asterisk inside it is not a delimiter. comrak needs `extension.math_dollars` for the same,
    /// and it applies micromark's constraint that the opening `$` is not followed by whitespace and
    /// the closing one is not preceded by whitespace.
    #[test]
    fn test_asterisks_inside_inline_math_are_not_emphasis() {
        assert!(md049_messages("_x_\n\n$X_1^*$ and $T_n^*$ here\n").is_empty());
        assert!(md049_messages("_x_\n\n$G^* = (T^*, E^*)$ of a graph\n").is_empty());
        // The `*` inside `\stackrel{*}` is math, so the trailing `*y*` is the only emphasis and it
        // is the asterisk half of a document whose style `_x_` set to underscore.
        assert_eq!(
            2,
            md049_messages("_x_\n\n$\\alpha \\stackrel{*}{\\Rightarrow} \\beta$ and *y*\n").len()
        );

        // A `$` amount is not a math delimiter, and neither is a `$` with whitespace inside it.
        assert_eq!(
            2,
            md049_messages("_x_\n\nprice $5 and $10 then *y*\n").len()
        );
        assert_eq!(2, md049_messages("_x_\n\n$ a_b $ not math *y*\n").len());

        // Math does not hide real emphasis beside it, and a code span still wins over math.
        assert_eq!(2, md049_messages("_x_\n\n$a$ plain *y*\n").len());
        assert_eq!(2, md049_messages("_x_\n\n`$a*$` code span *y*\n").len());
    }

    /// comrak caches where it last saw a backtick run of each length
    /// (`parser/inlines.rs:scan_to_closing_backtick`), and a *successful* scan overwrites the entry
    /// with the closer it found. Once an earlier unclosed run has set `scanned_for_backticks`, that
    /// stale entry makes every later opener of the same length look unclosed too, so `` `*b*` `` is
    /// left as text and its asterisks become delimiters. micromark rescans and keeps it a code
    /// span; markdownlint reports nothing here.
    #[test]
    fn code_spans_after_an_unclosed_run_are_a_known_difference() {
        assert_eq!(2, md049_messages("_x_\n\ny `` z `*a*` w `*b*` v\n").len());
    }
}
