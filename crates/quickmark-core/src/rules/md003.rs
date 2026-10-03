use crate::ast::Node;
use core::fmt;
use serde::Deserialize;
use std::rc::Rc;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{Rule, RuleType};

// MD003-specific configuration types
#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
pub enum HeadingStyle {
    #[serde(rename = "consistent")]
    Consistent,
    #[serde(rename = "atx")]
    ATX,
    #[serde(rename = "setext")]
    Setext,
    #[serde(rename = "atx_closed")]
    ATXClosed,
    #[serde(rename = "setext_with_atx")]
    SetextWithATX,
    #[serde(rename = "setext_with_atx_closed")]
    SetextWithATXClosed,
}

impl Default for HeadingStyle {
    fn default() -> Self {
        Self::Consistent
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD003HeadingStyleTable {
    #[serde(default)]
    pub style: HeadingStyle,
}

impl Default for MD003HeadingStyleTable {
    fn default() -> Self {
        Self {
            style: HeadingStyle::Consistent,
        }
    }
}

#[derive(PartialEq, Debug)]
enum Style {
    Setext,
    Atx,
    AtxClosed,
}

impl fmt::Display for Style {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Style::Setext => write!(f, "setext"),
            Style::Atx => write!(f, "atx"),
            Style::AtxClosed => write!(f, "atx_closed"),
        }
    }
}

pub(crate) struct MD003Linter {
    context: Rc<Context>,
    enforced_style: Option<Style>,
    violations: Vec<RuleViolation>,
}

impl MD003Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // Access MD003 config through the centralized config structure
        let md003_config = &context.config.linters.settings.heading_style;
        let enforced_style = match md003_config.style {
            HeadingStyle::ATX => Some(Style::Atx),
            HeadingStyle::Setext => Some(Style::Setext),
            HeadingStyle::ATXClosed => Some(Style::AtxClosed),
            HeadingStyle::SetextWithATX => None, // Allow both setext and atx
            HeadingStyle::SetextWithATXClosed => None, // Allow setext and atx_closed
            _ => None,
        };
        Self {
            context,
            enforced_style,
            violations: Vec::new(),
        }
    }

    fn get_heading_level(&self, node: &Node) -> u8 {
        let mut cursor = node.walk();
        match node.kind() {
            "atx_heading" => node
                .children(&mut cursor)
                .find_map(|child| {
                    let kind = child.kind();
                    if kind.starts_with("atx_h") && kind.ends_with("_marker") {
                        // "atx_h3_marker" -> 3
                        kind.get(5..6)?.parse::<u8>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(1),
            "setext_heading" => node
                .children(&mut cursor)
                .find_map(|child| match child.kind() {
                    "setext_h1_underline" => Some(1),
                    "setext_h2_underline" => Some(2),
                    _ => None,
                })
                .unwrap_or(1),
            _ => 1,
        }
    }

    fn add_violation(&mut self, node: &Node, expected: &str, actual: &Style) {
        self.violations.push(RuleViolation::new(
            &MD003,
            format!(
                "{} [Expected: {}; Actual: {}]",
                MD003.description, expected, actual
            ),
            self.context.file_path.clone(),
            range_from_node_range(&node.range()),
        ));
    }
}

impl RuleLinter for MD003Linter {
    fn feed(&mut self, node: &Node) {
        let style = match node.kind() {
            // markdownlint counts a heading's `atxHeadingSequence` tokens: one is `atx`, two is
            // `atx_closed`. A closing sequence has to be preceded by whitespace, so the `C#` in
            // `# Dissecting the async methods in C#` is text and the heading is plain `atx`.
            "atx_heading" => Some(if node.is_closed() {
                Style::AtxClosed
            } else {
                Style::Atx
            }),
            "setext_heading" => Some(Style::Setext),
            _ => None,
        };

        if let Some(style) = style {
            let level = self.get_heading_level(node);
            let config_style = &self.context.config.linters.settings.heading_style.style;

            match config_style {
                HeadingStyle::SetextWithATX => {
                    // Levels 1-2: must be setext, Levels 3+: must be atx (open), not atx_closed
                    if level <= 2 {
                        if style != Style::Setext {
                            self.add_violation(node, "setext", &style);
                        }
                    } else if style != Style::Atx {
                        self.add_violation(node, "atx", &style);
                    }
                }
                HeadingStyle::SetextWithATXClosed => {
                    // Levels 1-2: must be setext, Levels 3+: must be atx_closed, not plain atx
                    if level <= 2 {
                        if style != Style::Setext {
                            self.add_violation(node, "setext", &style);
                        }
                    } else if style != Style::AtxClosed {
                        self.add_violation(node, "atx_closed", &style);
                    }
                }
                _ => {
                    // For single-style configurations, check against enforced style
                    if let Some(enforced_style) = &self.enforced_style {
                        if style != *enforced_style {
                            self.add_violation(node, &enforced_style.to_string(), &style);
                        }
                    } else {
                        self.enforced_style = Some(style);
                    }
                }
            }
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD003: Rule = Rule {
    id: "MD003",
    aliases: &["heading-style"],
    tags: &["headings"],
    description: "Heading style",
    rule_type: RuleType::Token,
    required_nodes: &["atx_heading", "setext_heading"],
    new_linter: |context| Box::new(MD003Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use super::{HeadingStyle, MD003HeadingStyleTable};
    use crate::config::{LintersSettingsTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    const STYLES: [HeadingStyle; 6] = [
        HeadingStyle::Consistent,
        HeadingStyle::ATX,
        HeadingStyle::ATXClosed,
        HeadingStyle::Setext,
        HeadingStyle::SetextWithATX,
        HeadingStyle::SetextWithATXClosed,
    ];

    /// A report: the 1-based line, the style markdownlint expected and the one the heading has.
    type Want = (usize, &'static str, &'static str);
    type Found = (usize, String, String);

    /// A case's name, its document, and the reports markdownlint makes under each of [`STYLES`], in
    /// order.
    type Case<'a> = (&'a str, &'a str, [&'a [Want]; 6]);

    fn config(style: HeadingStyle) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![
                ("heading-style", RuleSeverity::Error),
                ("heading-increment", RuleSeverity::Off),
            ],
            LintersSettingsTable {
                heading_style: MD003HeadingStyleTable { style },
                ..Default::default()
            },
        )
    }

    fn owned(wants: &[Want]) -> Vec<Found> {
        wants
            .iter()
            .map(|&(line, expected, actual)| (line, expected.to_string(), actual.to_string()))
            .collect()
    }

    fn reports(style: HeadingStyle, source: &str) -> Vec<Found> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config(style), source);
        linter
            .analyze()
            .iter()
            .filter_map(|violation| {
                let (expected, actual) = violation
                    .message()
                    .split_once("[Expected: ")
                    .and_then(|(_, rest)| rest.split_once("; Actual: "))?;
                Some((
                    violation.location().range.start.line + 1,
                    expected.to_string(),
                    actual.trim_end_matches(']').to_string(),
                ))
            })
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3, under all six configured
    /// styles.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "a hash inside the text",
                "# Dissecting the async methods in C#\n\n## The generated\n",
                [
                    &[],
                    &[],
                    &[(1, "atx_closed", "atx"), (3, "atx_closed", "atx")],
                    &[(1, "setext", "atx"), (3, "setext", "atx")],
                    &[(1, "setext", "atx"), (3, "setext", "atx")],
                    &[(1, "setext", "atx"), (3, "setext", "atx")],
                ],
            ),
            (
                "open then closed",
                "# Open ATX\n## Closed ATX ##\n",
                [
                    &[(2, "atx", "atx_closed")],
                    &[(2, "atx", "atx_closed")],
                    &[(1, "atx_closed", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                ],
            ),
            (
                "text ending in a hash",
                "# Text ending with hash#\n## Second\n",
                [
                    &[],
                    &[],
                    &[(1, "atx_closed", "atx"), (2, "atx_closed", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                ],
            ),
            (
                "a long closing run",
                "### Unbalanced closing ########\n# Other\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "atx", "atx_closed"), (2, "setext", "atx")],
                    &[(2, "setext", "atx")],
                ],
            ),
            (
                "closed then open",
                "# H #\n## H\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                ],
            ),
            (
                "an empty closed heading",
                "# #\n## H\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                ],
            ),
            (
                "hashes only",
                "###\n# H\n",
                [
                    &[],
                    &[],
                    &[(1, "atx_closed", "atx"), (2, "atx_closed", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[(2, "setext", "atx")],
                    &[(1, "atx_closed", "atx"), (2, "setext", "atx")],
                ],
            ),
            (
                "setext then atx",
                "Setext\n======\n\n# Atx\n",
                [
                    &[(4, "setext", "atx")],
                    &[(1, "atx", "setext")],
                    &[(1, "atx_closed", "setext"), (4, "atx_closed", "atx")],
                    &[(4, "setext", "atx")],
                    &[(4, "setext", "atx")],
                    &[(4, "setext", "atx")],
                ],
            ),
            (
                "all closed",
                "# A #\n## B ##\n### C ###\n",
                [
                    &[],
                    &[
                        (1, "atx", "atx_closed"),
                        (2, "atx", "atx_closed"),
                        (3, "atx", "atx_closed"),
                    ],
                    &[],
                    &[
                        (1, "setext", "atx_closed"),
                        (2, "setext", "atx_closed"),
                        (3, "setext", "atx_closed"),
                    ],
                    &[
                        (1, "setext", "atx_closed"),
                        (2, "setext", "atx_closed"),
                        (3, "atx", "atx_closed"),
                    ],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx_closed")],
                ],
            ),
            (
                "open, closed and setext",
                "# A\n## B ##\nSetext\n======\n",
                [
                    &[(2, "atx", "atx_closed"), (3, "atx", "setext")],
                    &[(2, "atx", "atx_closed"), (3, "atx", "setext")],
                    &[(1, "atx_closed", "atx"), (3, "atx_closed", "setext")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                    &[(1, "setext", "atx"), (2, "setext", "atx_closed")],
                ],
            ),
            (
                "trailing spaces after the close",
                "## H ##   \n# G\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                ],
            ),
            (
                "plus signs in the text",
                "# C++\n## D\n",
                [
                    &[],
                    &[],
                    &[(1, "atx_closed", "atx"), (2, "atx_closed", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                ],
            ),
            (
                "a closing run longer than the opening",
                "# H ###\n## G\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                ],
            ),
            (
                "setext levels then atx levels",
                "Setext L1\n=========\nSetext L2\n---------\n### L3 atx\n#### L4 closed ####\n",
                [
                    &[(5, "setext", "atx"), (6, "setext", "atx_closed")],
                    &[
                        (1, "atx", "setext"),
                        (3, "atx", "setext"),
                        (6, "atx", "atx_closed"),
                    ],
                    &[
                        (1, "atx_closed", "setext"),
                        (3, "atx_closed", "setext"),
                        (5, "atx_closed", "atx"),
                    ],
                    &[(5, "setext", "atx"), (6, "setext", "atx_closed")],
                    &[(6, "atx", "atx_closed")],
                    &[(5, "atx_closed", "atx")],
                ],
            ),
            (
                "all open atx",
                "# a\n## b\n### c\n",
                [
                    &[],
                    &[],
                    &[
                        (1, "atx_closed", "atx"),
                        (2, "atx_closed", "atx"),
                        (3, "atx_closed", "atx"),
                    ],
                    &[
                        (1, "setext", "atx"),
                        (2, "setext", "atx"),
                        (3, "setext", "atx"),
                    ],
                    &[(1, "setext", "atx"), (2, "setext", "atx")],
                    &[
                        (1, "setext", "atx"),
                        (2, "setext", "atx"),
                        (3, "atx_closed", "atx"),
                    ],
                ],
            ),
            (
                "a tab after the closing run",
                "# H #\t\n## G\n",
                [
                    &[(2, "atx_closed", "atx")],
                    &[(1, "atx", "atx_closed")],
                    &[(2, "atx_closed", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                    &[(1, "setext", "atx_closed"), (2, "setext", "atx")],
                ],
            ),
        ];

        let mut failures = Vec::new();
        for (name, source, wants) in cases {
            for (index, style) in STYLES.iter().enumerate() {
                let (actual, expected) = (reports(*style, source), owned(wants[index]));
                if actual != expected {
                    failures.push(format!(
                        "{name} [{style:?}]: expected {expected:?}, got {actual:?}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}\n", failures.join("\n"));
    }
}
