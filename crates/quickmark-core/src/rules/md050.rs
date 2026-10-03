use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{md049::Marker, Rule, RuleType},
};

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

/// How wide a strong delimiter is: `**` and `__` are both two bytes.
const WIDTH: usize = 2;

pub(crate) struct MD050Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    document_style: Option<Marker>,
}

impl MD050Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            document_style: None,
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch. Pre-order matches markdownlint's token order, which is what makes
    /// `style = "consistent"` mean "whatever came first in the document".
    fn walk(&mut self, root: Node) {
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "strong_emphasis" {
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

        let expected = match self.context.config.linters.settings.strong_style.style {
            StrongStyle::Asterisk => Marker::Asterisk,
            StrongStyle::Underscore => Marker::Underscore,
            // The first strong emphasis in the document sets the style and is never a violation.
            StrongStyle::Consistent => *self.document_style.get_or_insert(marker),
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
        let message = format!("Expected: {}; Actual: {}", expected.name(), marker.name());
        let start_point = node.start_position();
        let end_point = node.end_position();
        for (marker_byte, from, to) in [
            (
                start,
                start_point,
                crate::ast::Point::new(start_point.row, start_point.column + WIDTH),
            ),
            (
                end - WIDTH,
                crate::ast::Point::new(end_point.row, end_point.column - WIDTH),
                end_point,
            ),
        ] {
            let range = crate::ast::NodeRange {
                start_byte: marker_byte,
                end_byte: marker_byte + WIDTH,
                start_point: from,
                end_point: to,
            };
            self.violations.push(RuleViolation::new(
                &MD050,
                message.clone(),
                self.context.file_path.clone(),
                range_from_node_range(&range),
            ));
        }
    }

    /// Which marker a `strong_emphasis` node was written with. The node's own source starts at its
    /// opening delimiter, so the first byte settles it.
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

impl RuleLinter for MD050Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.walk(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD050: Rule = Rule {
    id: "MD050",
    aliases: &["strong-style"],
    tags: &["emphasis"],
    description: "Strong style",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
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

    /// One report: markdownlint's line and column, and the styles it names. Its `errorRange` covers
    /// the delimiter, so the opening and the closing one are two reports.
    type Report<'a> = (usize, usize, &'a str);

    /// The same report with an owned message, which is what a violation hands back.
    type Found = (usize, usize, String);

    fn owned(expected: &[Report<'_>]) -> Vec<Found> {
        expected
            .iter()
            .map(|&(line, column, detail)| (line, column, detail.to_string()))
            .collect()
    }

    fn found(source: &str) -> Vec<Found> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    violation.message().to_string(),
                )
            })
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD050's defaults.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, and every case below is ASCII, so the
    /// two agree throughout.
    const CASES: &[(&str, &[Report])] = &[
        (
            "**a** and __b__\n",
            &[
                (1, 11, "Expected: asterisk; Actual: underscore"),
                (1, 14, "Expected: asterisk; Actual: underscore"),
            ],
        ),
        ("__a__\n", &[]),
        ("**a**\n", &[]),
        ("std::__1::future_error\n", &[]),
        ("(__syncthreads)\n", &[]),
        ("__libc_start_main()\n", &[]),
        ("a__b__c\n", &[]),
        (
            "**a** __b__\n",
            &[
                (1, 7, "Expected: asterisk; Actual: underscore"),
                (1, 10, "Expected: asterisk; Actual: underscore"),
            ],
        ),
        ("___a___\n", &[]),
        ("**_a_**\n", &[]),
        ("_**a**_\n", &[]),
        (
            "__a__ **b**\n",
            &[
                (1, 7, "Expected: underscore; Actual: asterisk"),
                (1, 10, "Expected: underscore; Actual: asterisk"),
            ],
        ),
        ("`__a__`\n", &[]),
        ("$__a__$\n", &[]),
        ("[__a__](http://x)\n", &[]),
        ("http://x/__a__\n", &[]),
        ("<div>__a__</div>\n", &[]),
        (
            "**a**\n\n__b__\n",
            &[
                (3, 1, "Expected: asterisk; Actual: underscore"),
                (3, 4, "Expected: asterisk; Actual: underscore"),
            ],
        ),
        ("__a__ *b* __c__\n", &[]),
        ("x __a__\n", &[]),
        (
            "__a__\n\n**b** **c**\n",
            &[
                (3, 1, "Expected: underscore; Actual: asterisk"),
                (3, 4, "Expected: underscore; Actual: asterisk"),
                (3, 7, "Expected: underscore; Actual: asterisk"),
                (3, 10, "Expected: underscore; Actual: asterisk"),
            ],
        ),
        (
            "**a**\n\n__b__\n\n__c__\n",
            &[
                (3, 1, "Expected: asterisk; Actual: underscore"),
                (3, 4, "Expected: asterisk; Actual: underscore"),
                (5, 1, "Expected: asterisk; Actual: underscore"),
                (5, 4, "Expected: asterisk; Actual: underscore"),
            ],
        ),
        (
            "- __a__\n- **b**\n",
            &[
                (2, 3, "Expected: underscore; Actual: asterisk"),
                (2, 6, "Expected: underscore; Actual: asterisk"),
            ],
        ),
        (
            "> __a__\n>\n> **b**\n",
            &[
                (3, 3, "Expected: underscore; Actual: asterisk"),
                (3, 6, "Expected: underscore; Actual: asterisk"),
            ],
        ),
        ("| __a__ |\n|---|\n", &[]),
        ("# __a__\n", &[]),
        ("__a\nb__\n", &[]),
        ("***a***\n", &[]),
        ("**a*b**\n", &[]),
        ("__**a**__\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(owned(expected), found(source), "source {source:?}");
        }
    }
}
