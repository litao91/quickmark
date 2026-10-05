use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{ellipsify, label_span, Context, Label, Rule, RuleLinter, RuleType},
};

// MD059-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD059DescriptiveLinkTextTable {
    // Not `#[serde(default)]`: a settings table that sets nothing still gets the default prohibited
    // texts, which is what markdownlint's `config.prohibited_texts || [...]` does.
    #[serde(default = "default_prohibited_texts")]
    pub prohibited_texts: Vec<String>,
}

fn default_prohibited_texts() -> Vec<String> {
    vec![
        "click here".to_string(),
        "here".to_string(),
        "link".to_string(),
        "more".to_string(),
    ]
}

impl Default for MD059DescriptiveLinkTextTable {
    fn default() -> Self {
        Self {
            prohibited_texts: default_prohibited_texts(),
        }
    }
}

/// markdownlint's `normalize`, first half: every run of characters that is not an ASCII letter or
/// digit becomes one space. Spelled out rather than as markdownlint's `[\W_]`, which is ASCII-only
/// in a JavaScript pattern without the `u` flag and so cannot be written that way here — a
/// byte-oriented `\W` would match half of a multi-byte character.
static NOT_ALPHANUMERIC: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^A-Za-z0-9]+").expect("Invalid MD059 punctuation regex"));
static WHITESPACE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s+").expect("Invalid MD059 whitespace regex"));

fn normalize_text(text: &str) -> String {
    let punctuated = NOT_ALPHANUMERIC.replace_all(text, " ");
    WHITESPACE
        .replace_all(&punctuated, " ")
        .to_lowercase()
        .trim()
        .to_string()
}

/// MD059 - Link text should be descriptive
///
/// Reports a link whose label is nothing but one of the prohibited texts.
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

    fn collect(&mut self, root: Node) {
        let found = {
            let source = self.context.document_content.borrow();
            self.links(root, &source)
        };
        self.violations.extend(found);
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch. Pre-order, so a link inside a link's label comes after the one containing it.
    fn links(&self, root: Node, source: &str) -> Vec<RuleViolation> {
        let mut found = Vec::new();
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "link" {
                found.extend(self.check(node, source));
            }
            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
                    return found;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return found;
                }
                depth -= 1;
            }
        }
    }

    /// An image is not a link, and neither is a reference that resolved to nothing — the parser
    /// leaves both out of the tree, so `[here]` with no definition is simply not here to find.
    fn check(&self, link: Node, source: &str) -> Option<RuleViolation> {
        let label = label_span(link, source)?;
        // markdownlint skips a label holding a code span or inline HTML, because neither reads aloud
        // as its own source.
        let mut cursor = link.walk();
        let holds_markup = link
            .children(&mut cursor)
            .any(|child| matches!(child.kind(), "code_span" | "html_inline"));
        if holds_markup {
            return None;
        }
        let text = source.get(label.from..label.to)?;
        self.prohibited_texts
            .contains(&normalize_text(text))
            .then(|| self.violation(label, source))
    }

    fn violation(&self, label: Label, source: &str) -> RuleViolation {
        // markdownlint quotes the label brackets and all, and underlines only what is between them —
        // micromark's `labelText` token stops at the `]`.
        let quoted = source.get(label.from - 1..=label.to).unwrap_or_default();
        let start = self.context.point_at(label.from);
        let end_byte = {
            let lines = self.context.lines.borrow();
            let line_end = self.context.line_start_byte(start.row) + lines[start.row].len();
            label.to.min(line_end)
        };
        RuleViolation::new(
            &MD059,
            format!(
                "{} [Context: \"{}\"]",
                MD059.description,
                ellipsify(quoted, false, false)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: label.from,
                end_byte,
                start_point: start,
                end_point: self.context.point_at(end_byte),
            }),
        )
    }
}

impl RuleLinter for MD059Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.collect(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD059: Rule = Rule {
    id: "MD059",
    aliases: &["descriptive-link-text"],
    tags: &["accessibility", "links"],
    description: "Link text should be descriptive",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD059Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD059DescriptiveLinkTextTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    use super::normalize_text;

    /// A report: the 1-based line and column of the label's text, and the label markdownlint quotes
    /// brackets and all.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);
    type Case = (&'static str, &'static str, &'static [Report]);

    fn test_config(prohibited: &[&str]) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("descriptive-link-text", RuleSeverity::Error)],
            LintersSettingsTable {
                descriptive_link_text: MD059DescriptiveLinkTextTable {
                    prohibited_texts: prohibited.iter().map(|&text| text.to_string()).collect(),
                },
                ..Default::default()
            },
        )
    }

    const DEFAULT: [&str; 4] = ["click here", "here", "link", "more"];

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, context)| (line, column, context.to_string()))
            .collect()
    }

    fn reports(input: &str, prohibited: &[&str]) -> Vec<Found> {
        let config = test_config(prohibited);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let mut found: Vec<Found> = linter
            .analyze()
            .iter()
            .map(|violation| {
                let context = violation
                    .message()
                    .split_once("[Context: \"")
                    .and_then(|(_, rest)| rest.strip_suffix("\"]"))
                    .unwrap_or_default();
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    context.to_string(),
                )
            })
            .collect();
        found.sort();
        found
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3 with its default prohibited
    /// texts, which are also this crate's.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "a link with a prohibited label",
                "[here](https://x.com)\n",
                &[(1, 2, "[here]")],
            ),
            (
                "a two word prohibited label",
                "[click here](https://x.com)\n",
                &[(1, 2, "[click here]")],
            ),
            (
                "a prohibited label in title case",
                "[Click Here](https://x.com)\n",
                &[(1, 2, "[Click Here]")],
            ),
            ("the word link", "[link](x)\n", &[(1, 2, "[link]")]),
            ("the word more", "[more](x)\n", &[(1, 2, "[more]")]),
            (
                "a label that only contains a prohibited word",
                "[a link](x)\n",
                &[],
            ),
            (
                "two links on one line",
                "[here](x) and [here](y)\n",
                &[(1, 2, "[here]"), (1, 16, "[here]")],
            ),
            ("an image with a prohibited label", "![here](x.png)\n", &[]),
            (
                "a full reference link",
                "[here][ref]\n\n[ref]: /u\n",
                &[(1, 2, "[here]")],
            ),
            (
                "a collapsed reference link",
                "[here][]\n\n[here]: /u\n",
                &[(1, 2, "[here]")],
            ),
            (
                "a shortcut reference link",
                "[here]\n\n[here]: /u\n",
                &[(1, 2, "[here]")],
            ),
            ("a label that is a code span", "[`here`](x)\n", &[]),
            ("a label holding inline html", "[<b>here</b>](x)\n", &[]),
            (
                "a code span nested in an emphasis",
                "[a *`here`* b](x)\n",
                &[],
            ),
            ("inside a code span", "`[here](x)`\n", &[]),
            ("inside a fenced code block", "```\n[here](x)\n```\n", &[]),
            ("in a heading", "# [here](x)\n", &[(1, 4, "[here]")]),
            (
                "in a table cell",
                "| a |\n| - |\n| [here](x) |\n",
                &[(3, 4, "[here]")],
            ),
            ("in a block quote", "> [here](x)\n", &[(1, 4, "[here]")]),
            ("in a list item", "- [here](x)\n", &[(1, 4, "[here]")]),
            ("a label of single letters", "[h.e.r.e](x)\n", &[]),
            (
                "a label ending in punctuation",
                "[here!](x)\n",
                &[(1, 2, "[here!]")],
            ),
            ("a padded label", "[ here ](x)\n", &[(1, 2, "[ here ]")]),
            ("a label in upper case", "[HERE](x)\n", &[(1, 2, "[HERE]")]),
            (
                "inside strong emphasis",
                "**[here](x)**\n",
                &[(1, 4, "[here]")],
            ),
            ("a label holding brackets", "[a[here]b](x)\n", &[]),
            ("an autolink", "<https://x.com>\n", &[]),
            (
                "a link with a title",
                "[here](x \"here\")\n",
                &[(1, 2, "[here]")],
            ),
            (
                "a full reference to its own label",
                "[here][here]\n\n[here]: /u\n",
                &[(1, 2, "[here]")],
            ),
            (
                "the word link in title case",
                "[Link](x)\n",
                &[(1, 2, "[Link]")],
            ),
            ("an image inside a link label", "[![here](y.png)](x)\n", &[]),
            (
                "a label that is an emphasis",
                "[*here*](x)\n",
                &[(1, 2, "[*here*]")],
            ),
            ("inside an html block", "<div>\n[here](x)\n</div>\n", &[]),
            ("a label with an accent", "[café](x)\n", &[]),
            (
                "a label with runs of spaces",
                "[  click   here  ](x)\n",
                &[(1, 2, "[  click   here  ]")],
            ),
            (
                "two links in two paragraphs",
                "[here](x)\n\n[here](y)\n",
                &[(1, 2, "[here]"), (3, 2, "[here]")],
            ),
            (
                "a shortcut reference with no definition",
                "text [here] text\n",
                &[],
            ),
            (
                "before a hard line break",
                "[here](x)\\\ntext\n",
                &[(1, 2, "[here]")],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input, &DEFAULT), "{name}");
        }
    }

    /// A configured list replaces the default rather than adding to it, and is normalized the same
    /// way the labels are.
    #[test]
    fn a_configured_list_replaces_the_default() {
        let prohibited = ["download", "read more"];
        let cases: &[Case] = &[
            ("one word", "[Download](x)\n", &[(1, 2, "[Download]")]),
            ("as part of a longer label", "[download it](x)\n", &[]),
            ("a default that is no longer one", "[here](x)\n", &[]),
            ("two words", "[Read More](x)\n", &[(1, 2, "[Read More]")]),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input, &prohibited), "{name}");
        }
    }

    /// markdownlint gives a label spanning lines no range at all, so markdownlint-cli2 prints no
    /// column — but it still quotes the whole label, with every line break turned into a space. The
    /// report here keeps the line and the context, and its range stops at the end of that line.
    #[test]
    fn a_label_spanning_lines_is_quoted_as_one() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("[here\n](x)\n", &DEFAULT, "[here ]"),
            ("[click\nhere](x)\n", &DEFAULT, "[click here]"),
            ("[read\nmore](x)\n", &["read more"], "[read more]"),
        ];
        for (input, prohibited, context) in cases {
            assert_eq!(
                vec![(1, 2, context.to_string())],
                reports(input, prohibited),
                "{input:?}"
            );
        }

        let config = test_config(&DEFAULT);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, "[here\n](x)\n");
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 1, 0, 5),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }

    /// A case's name, its document, and the `(line, column, width)` of each report, all 1-based.
    type RangeCase = (&'static str, &'static str, &'static [(usize, usize, usize)]);

    fn ranges(input: &str) -> Vec<(usize, usize, usize)> {
        let config = test_config(&DEFAULT);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                )
            })
            .collect()
    }

    /// markdownlint underlines micromark's `labelText` — what sits between the brackets, not the
    /// brackets or the `]` after it.
    /// Every tuple is markdownlint v0.41.1's `errorRange`, read back through its `lintSync` API.
    #[test]
    fn matches_markdownlints_error_range() {
        let cases: &[RangeCase] = &[
            ("a link label used once", "[here](x)\n", &[(1, 2, 4)]),
            ("a label with a space", "[click here](x)\n", &[(1, 2, 10)]),
            ("an indented label", "  [here](x)\n", &[(1, 4, 4)]),
            ("an image label is not this rule's", "![here](x)\n", &[]),
            ("a reference label", "[here][r]\n\n[r]: /u\n", &[(1, 2, 4)]),
        ];
        for &(name, source, expected) in cases {
            assert_eq!(expected, ranges(source).as_slice(), "{name}");
        }
    }

    #[test]
    fn normalize_collapses_punctuation_and_case() {
        assert_eq!("click here", normalize_text("click here"));
        assert_eq!("click here", normalize_text("Click Here"));
        assert_eq!("click here", normalize_text("click   here"));
        assert_eq!("click here", normalize_text("click_here"));
        assert_eq!("click here", normalize_text("click-here"));
        assert_eq!("click here", normalize_text("  click here  "));
        assert_eq!("click here", normalize_text("click.here!"));
        // `\W` is ASCII-only in markdownlint's pattern, so an accent is punctuation.
        assert_eq!("caf", normalize_text("café"));
    }
}
