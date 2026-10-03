use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

/// Inline markup micromark tokenises on its own but comrak leaves inside a `text` node, which is
/// what stops markdownlint reporting an emphasis that holds one. A bare URL or email is a GFM
/// autolink literal and quickmark parses without that extension; a character reference comrak
/// decodes and merges into the text around it.
static NOT_PLAIN_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?:www\.|[A-Za-z][A-Za-z0-9+.-]*://)[^<>\s]",
        r"|[^<>\s@]+@[^<>\s@]+\.[A-Za-z]{2,}",
        r"|&(?:#[0-9]+|#[xX][0-9a-fA-F]+|[A-Za-z][A-Za-z0-9]{1,31});",
    ))
    .expect("Invalid MD036 markup regex")
});

/// markdownlint only considers a paragraph whose micromark parent is the document's `content`
/// chunk, which excludes anything nested in a list item, block quote, table cell or HTML block.
/// The facade wraps document-level blocks in `section` nodes, so those are the only other ancestors
/// a candidate paragraph may have.
fn is_document_level(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if !matches!(ancestor.kind(), "section" | "document") {
            return false;
        }
        current = ancestor.parent();
    }
    true
}

/// Whether the text holds a backslash escape. CommonMark escapes any ASCII punctuation, and a
/// backslash before anything else is a literal character that stays in the surrounding text.
fn has_escape(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(rest) = bytes.get(1..) else {
        return false;
    };
    bytes
        .iter()
        .zip(rest)
        .any(|(&left, &right)| left == b'\\' && right.is_ascii_punctuation())
}

/// Whether a paragraph child counts, in markdownlint's sense. An inline HTML tag does not, and
/// neither does a run of whitespace that micromark tokenises as an empty `data`. Everything else
/// does — including a soft line break and the trailing whitespace a `lineSuffix` covers, neither of
/// which the facade has a node for, so [`MD036Linter::emphasis_heading`] looks for both itself.
fn is_meaningful(node: Node, source: &str) -> bool {
    match node.kind() {
        "html_inline" => false,
        "text" => !source[node.start_byte()..node.end_byte()].trim().is_empty(),
        _ => true,
    }
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

    fn check(&mut self, paragraph: Node) {
        if !is_document_level(paragraph) {
            return;
        }
        // A paragraph holds a single inline node covering all of its content.
        let Some(inline) = paragraph.named_child(0) else {
            return;
        };
        if inline.kind() != "inline" {
            return;
        }

        let text = {
            let source = self.context.get_document_content();
            self.emphasis_heading(inline, &source)
        };
        let Some(text) = text else {
            return;
        };

        self.violations.push(RuleViolation::new(
            &MD036,
            format!(
                "{} [Context: \"{}\"]",
                MD036.description,
                ellipsify(&text, false, false)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&inline.range()),
        ));
    }

    /// The text of the emphasis a paragraph consists of, when there is one.
    ///
    /// markdownlint asks for a paragraph with exactly one child that is neither an inline HTML tag
    /// nor whitespace, and then for an `emphasis` or `strong` among its children whose own content
    /// is a single `data` token. The facade emits no marker nodes, so an emphasis's children are
    /// exactly its content's.
    fn emphasis_heading(&self, inline: Node, source: &str) -> Option<String> {
        // A soft line break is a child of its own, so a paragraph spread over more than one line
        // has at least two and never qualifies.
        if inline.start_position().row != inline.end_position().row {
            return None;
        }
        // So is the whitespace a `lineSuffix` covers at the end of the line. It sits outside the
        // inline, whose end skips it — except at EOF without a newline, where it does not, so this
        // reads the line rather than the inline.
        let from = inline.start_byte();
        let line_end = source[from..]
            .find(['\n', '\r'])
            .map_or(source.len(), |offset| from + offset);
        if source[from..line_end].ends_with([' ', '\t']) {
            return None;
        }

        let mut cursor = inline.walk();
        let meaningful: Vec<Node> = inline
            .children(&mut cursor)
            .filter(|&child| is_meaningful(child, source))
            .collect();
        let [emphasis] = meaningful.as_slice() else {
            return None;
        };
        if !matches!(emphasis.kind(), "emphasis" | "strong_emphasis") {
            return None;
        }
        if emphasis.child_count() != 1 {
            return None;
        }
        let inner = emphasis.child(0)?;
        if inner.kind() != "text" {
            return None;
        }

        let text = &source[inner.start_byte()..inner.end_byte()];
        // micromark also splits `data` around a `[` that opens a link label — even one that never
        // closes — and around a backslash escape, so an emphasis holding either has more than one
        // child there and none here.
        if text.contains('[') || has_escape(text) || NOT_PLAIN_TEXT.is_match(text) {
            return None;
        }
        let punctuation = &self
            .context
            .config
            .linters
            .settings
            .emphasis_as_heading
            .punctuation;
        // A sentence ending in punctuation reads as prose, not as a heading.
        if text
            .chars()
            .next_back()
            .is_some_and(|last| punctuation.contains(last))
        {
            return None;
        }

        Some(text.to_string())
    }
}

impl RuleLinter for MD036Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "paragraph" {
            self.check(*node);
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

    /// A report: the 1-based line and the context markdownlint quotes. It gives no column, because
    /// the rule passes `addErrorContext` no range.
    type Report = (usize, &'static str);
    type Found = (usize, String);
    type Case = (&'static str, &'static str, &'static [Report]);

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

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, context)| (line, context.to_string()))
            .collect()
    }

    fn reports(input: &str, punctuation: &str) -> Vec<Found> {
        let config = test_config(punctuation);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let context = violation
                    .message()
                    .split_once("[Context: \"")
                    .and_then(|(_, rest)| rest.strip_suffix("\"]"))
                    .unwrap_or_default();
                (
                    violation.location().range.start.line + 1,
                    context.to_string(),
                )
            })
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3, which reports a paragraph whose
    /// one meaningful child is an emphasis holding a single run of plain text.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "a strong paragraph on its own",
                "**Section 1**\n\nContent.\n",
                &[(1, "Section 1")],
            ),
            (
                "an emphasized paragraph on its own",
                "*Section 1*\n\nContent.\n",
                &[(1, "Section 1")],
            ),
            (
                "an underscored paragraph on its own",
                "_Section 1_\n\nContent.\n",
                &[(1, "Section 1")],
            ),
            (
                "a double underscored paragraph on its own",
                "__Section 1__\n\nContent.\n",
                &[(1, "Section 1")],
            ),
            (
                "a strong emphasis inside an emphasis",
                "***Section 1***\n\nContent.\n",
                &[],
            ),
            (
                "an emphasis inside a strong emphasis",
                "**_Section 1_**\n\nContent.\n",
                &[],
            ),
            (
                "emphasis inside a longer paragraph",
                "This is **not** a heading\n",
                &[],
            ),
            ("ending in punctuation", "**Ends with.**\n\nContent.\n", &[]),
            (
                "followed by trailing whitespace",
                "**x** \n\nContent.\n",
                &[],
            ),
            (
                "indented three spaces",
                "  **x**\n\nContent.\n",
                &[(1, "x")],
            ),
            ("spanning two lines", "**multi\nline**\n\nContent.\n", &[]),
            ("in a block quote", "> **x**\n\nContent.\n", &[]),
            ("in a list item", "- **x**\n\nContent.\n", &[]),
            ("holding a link", "**[a](b)**\n\nContent.\n", &[]),
            ("holding a code span", "**`a`**\n\nContent.\n", &[]),
            ("holding math", "**$a$**\n\nContent.\n", &[]),
            (
                "inside inline html",
                "<b>**x**</b>\n\nContent.\n",
                &[(1, "x")],
            ),
            ("beside inline html", "**x** <b>y</b>\n\nContent.\n", &[]),
            (
                "holding an unpaired asterisk",
                "**a*b**\n\nContent.\n",
                &[(1, "a*b")],
            ),
            (
                "holding an unpaired underscore",
                "**a_b**\n\nContent.\n",
                &[(1, "a_b")],
            ),
            (
                "holding underscores between words",
                "**snake_case_name**\n\nContent.\n",
                &[(1, "snake_case_name")],
            ),
            (
                "holding a bare ampersand",
                "**AT&T**\n\nContent.\n",
                &[(1, "AT&T")],
            ),
            (
                "holding a character reference",
                "**&amp;**\n\nContent.\n",
                &[],
            ),
            ("holding a backslash escape", "**a\\_b**\n\nContent.\n", &[]),
            (
                "holding a bare url",
                "**see www.example.com now**\n\nContent.\n",
                &[],
            ),
            (
                "holding an unclosed link label",
                "**a[b**\n\nContent.\n",
                &[],
            ),
            ("holding unpaired tildes", "** ~~a~~ **\n\nContent.\n", &[]),
            ("inside an html block", "<div>\n**x**\n</div>\n", &[]),
            (
                "ending in full width punctuation",
                "**Section 1。**\n\nContent.\n",
                &[],
            ),
            ("four asterisks", "****\n\nContent.\n", &[]),
            ("asterisks around a space", "** **\n\nContent.\n", &[]),
            (
                "two paragraphs",
                "**one**\n\n**two**\n\nContent.\n",
                &[(1, "one"), (3, "two")],
            ),
            (
                "two lines each holding an emphasis",
                "**a**\n**b**\n\nContent.\n",
                &[],
            ),
            (
                "before a link reference definition",
                "**x**\n\n[a]: /u\n",
                &[(1, "x")],
            ),
            ("in a table cell", "| a |\n| - |\n| **x** |\n", &[]),
            ("two emphases side by side", "*a* *b*\n\nContent.\n", &[]),
            ("followed by a trailing tab", "**x**\t\n\nContent.\n", &[]),
            ("an emphasis closed early", "***a*b**\n\nContent.\n", &[]),
            ("underscores between letters", "a_b_c\n\nContent.\n", &[]),
            (
                "in a fenced code block",
                "```\n**x**\n```\n\nContent.\n",
                &[],
            ),
            (
                "after front matter",
                "---\ntitle: x\n---\n\n**x**\n",
                &[(5, "x")],
            ),
            (
                "an emphasis with text after it",
                "**a**b**\n\nContent.\n",
                &[],
            ),
            ("alone in the document", "**x**\n", &[(1, "x")]),
            (
                "asterisks around padded text",
                "**  spaced  **\n\nContent.\n",
                &[],
            ),
            (
                "holding an inline html tag",
                "**a <b> b**\n\nContent.\n",
                &[],
            ),
            (
                "an emphasis closed on the next line",
                "**a\n**\n\nContent.\n",
                &[],
            ),
            (
                "after inline html and a space",
                "<b> **x**</b>\n\nContent.\n",
                &[(1, "x")],
            ),
            (
                "before a space and inline html",
                "<b>**x** </b>\n\nContent.\n",
                &[(1, "x")],
            ),
            (
                "between blank lines in an html block",
                "<div>\n\n**x**\n\n</div>\n",
                &[(3, "x")],
            ),
            (
                "longer than thirty characters",
                "**a heading that is definitely longer than thirty characters**\n\nContent.\n",
                &[(1, "a heading that is definitely l...")],
            ),
            (
                "a short one and a punctuated one",
                "_short_\n\n**exactly thirty characters long!!**\n",
                &[(1, "short")],
            ),
            (
                "holding an escaped backslash",
                "**a\\\\b**\n\nContent.\n",
                &[],
            ),
            (
                "holding a backslash before a letter",
                "**a\\db**\n\nContent.\n",
                &[(1, "a\\db")],
            ),
            (
                "holding a reference without its semicolon",
                "**&amp**\n\nContent.\n",
                &[(1, "&amp")],
            ),
            (
                "holding a numeric character reference",
                "**a&#35;b**\n\nContent.\n",
                &[],
            ),
            (
                "holding an angle bracket autolink",
                "**<https://x.com>**\n\nContent.\n",
                &[],
            ),
            (
                "holding an unclosed image label",
                "**a ![b**\n\nContent.\n",
                &[],
            ),
            ("holding a bare email", "**foo@bar.com**\n\nContent.\n", &[]),
            (
                "with carriage returns",
                "**x**\r\n\r\nContent.\r\n",
                &[(1, "x")],
            ),
            ("with trailing spaces and no final newline", "**x**   ", &[]),
            ("with no final newline", "**x**", &[(1, "x")]),
            ("followed by a hard line break", "**x**  \ny\n", &[]),
            ("with text after it", "**x**y\n\nContent.\n", &[]),
            (
                "holding a bare dollar",
                "**a$b**\n\nContent.\n",
                &[(1, "a$b")],
            ),
            (
                "holding a bare angle bracket",
                "**a<b**\n\nContent.\n",
                &[(1, "a<b")],
            ),
            (
                "before an html comment",
                "**a**\n\n<!-- c -->\n",
                &[(1, "a")],
            ),
            ("holding a url", "**https://x.com/a**\n\nContent.\n", &[]),
            ("holding a www url", "**www.x.com**\n\nContent.\n", &[]),
            ("followed by a backslash hard break", "**a**\\\nb\n", &[]),
            ("indented with a tab", "\t**x**\n\nContent.\n", &[]),
            (
                "with trailing spaces at the end of the file",
                "**x**  \n\nContent.\n",
                &[],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(
                owned(expected),
                reports(input, ".,;:!?。，；：！？"),
                "{name}"
            );
        }
    }

    /// A configured set replaces the default rather than adding to it, so the full-width exclamation
    /// mark below stops suppressing a report.
    #[test]
    fn a_configured_set_replaces_the_default() {
        let cases: &[Case] = &[
            (
                "an exclamation mark",
                "**This heading has exclamation!**\n\nContent.\n",
                &[(1, "This heading has exclamation!")],
            ),
            (
                "a period",
                "**This heading has period.**\n\nContent.\n",
                &[],
            ),
            (
                "no punctuation at all",
                "**Empty punctuation**\n\nContent.\n",
                &[(1, "Empty punctuation")],
            ),
            (
                "a full-width exclamation mark",
                "**Trailing ！**\n\nContent.\n",
                &[(1, "Trailing ！")],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input, ".,;:"), "{name}");
        }
    }

    #[test]
    fn a_report_covers_the_inline() {
        let config = test_config(".,;:!?。，；：！？");
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, "  **x**\n");
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 0, 0, 7),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }
}
