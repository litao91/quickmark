use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

// MD029-specific configuration types
#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
pub enum OlPrefixStyle {
    #[serde(rename = "one")]
    One,
    #[serde(rename = "ordered")]
    Ordered,
    #[serde(rename = "one_or_ordered")]
    OneOrOrdered,
    #[serde(rename = "zero")]
    Zero,
}

impl Default for OlPrefixStyle {
    fn default() -> Self {
        Self::OneOrOrdered
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD029OlPrefixTable {
    #[serde(default)]
    pub style: OlPrefixStyle,
}

impl Default for MD029OlPrefixTable {
    fn default() -> Self {
        Self {
            style: OlPrefixStyle::OneOrOrdered,
        }
    }
}

pub(crate) struct MD029Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

/// The style a message names. `one_or_ordered` is resolved per list before anything is reported, so
/// it never reaches one.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Style {
    One,
    Ordered,
    Zero,
}

impl Style {
    fn example(self) -> &'static str {
        match self {
            Self::One => "1/1/1",
            Self::Ordered => "1/2/3",
            Self::Zero => "0/0/0",
        }
    }
}

impl MD029Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// An ordered list's own items — each one's marker and the number it shows — or `None` when the
    /// list is unordered. Only direct items count, so a nested list is left to its own visit and
    /// keeps its own numbering.
    fn ordered_items<'a>(&self, node: Node<'a>) -> Option<Vec<(Node<'a>, u32)>> {
        let source = self.context.document_content.borrow();
        let mut items = Vec::new();
        let mut cursor = node.walk();
        for item in node.children(&mut cursor) {
            if item.kind() != "list_item" {
                continue;
            }
            let mut marker_cursor = item.walk();
            let marker = item.children(&mut marker_cursor).find(|child| {
                matches!(child.kind(), "list_marker_dot" | "list_marker_parenthesis")
            })?;
            items.push((marker, marker_value(marker, &source)?));
        }
        (!items.is_empty()).then_some(items)
    }

    fn check(&mut self, node: Node) {
        let Some(items) = self.ordered_items(node) else {
            return;
        };

        // markdownlint reads the numbering off the first two items: a list increments when its
        // second item is not 1, or when it starts at 0 — which also starts the expected sequence
        // at 0. A single item shows no pattern and is held to 1.
        let mut expected = 1;
        let mut incrementing = false;
        if let [first, second, ..] = items.as_slice() {
            if second.1 != 1 || first.1 == 0 {
                incrementing = true;
                if first.1 == 0 {
                    expected = 0;
                }
            }
        }

        let style = match self.context.config.linters.settings.ol_prefix.style {
            OlPrefixStyle::One => Style::One,
            OlPrefixStyle::Ordered => Style::Ordered,
            OlPrefixStyle::Zero => Style::Zero,
            OlPrefixStyle::OneOrOrdered if incrementing => Style::Ordered,
            OlPrefixStyle::OneOrOrdered => Style::One,
        };
        // A configured `one` or `zero` overrides whatever the items suggested. `ordered` does not,
        // which is how a list that starts at 0 comes to be checked against 0/1/2.
        match style {
            Style::One => expected = 1,
            Style::Zero => expected = 0,
            Style::Ordered => {}
        }

        for (marker, actual) in items {
            if actual != expected {
                self.report(marker, expected, actual, style);
            }
            if style == Style::Ordered {
                expected = expected.saturating_add(1);
            }
        }
    }

    fn report(&mut self, marker: Node, expected: u32, actual: u32, style: Style) {
        self.violations.push(RuleViolation::new(
            &MD029,
            format!(
                "{} [Expected: {expected}; Actual: {actual}; Style: {}]",
                MD029.description,
                style.example()
            ),
            self.context.file_path.clone(),
            range_from_node_range(&marker.range()),
        ));
    }
}

/// The number an ordered marker shows. The marker spans the digits, the delimiter and the space
/// after it, so `1. ` and `3) ` both reduce to their digits.
fn marker_value(marker: Node, source: &str) -> Option<u32> {
    let text = marker.utf8_text(source.as_bytes()).ok()?;
    text.trim_end().trim_end_matches(['.', ')']).parse().ok()
}

impl RuleLinter for MD029Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "list" {
            self.check(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD029: Rule = Rule {
    id: "MD029",
    aliases: &["ol-prefix"],
    tags: &["ol"],
    description: "Ordered list item prefix",
    rule_type: RuleType::Token,
    required_nodes: &["list"],
    new_linter: |context| Box::new(MD029Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{
        LintersSettingsTable, LintersTable, MD029OlPrefixTable, OlPrefixStyle, QuickmarkConfig,
        RuleSeverity,
    };
    use crate::linter::MultiRuleLinter;

    const ONE: &str = "1/1/1";
    const ORDERED: &str = "1/2/3";
    const ZERO: &str = "0/0/0";

    /// A report: the 1-based line, the number markdownlint expected, the one the document shows,
    /// and the style example named in the message.
    type Want = (usize, u32, u32, &'static str);
    type Found = (usize, u32, u32, String);

    /// A case's name, its document, and the reports markdownlint makes under `one_or_ordered`,
    /// `one`, `ordered` and `zero`, in that order.
    type Case<'a> = (&'a str, &'a str, [&'a [Want]; 4]);

    const STYLES: [OlPrefixStyle; 4] = [
        OlPrefixStyle::OneOrOrdered,
        OlPrefixStyle::One,
        OlPrefixStyle::Ordered,
        OlPrefixStyle::Zero,
    ];

    fn config(style: OlPrefixStyle) -> QuickmarkConfig {
        QuickmarkConfig::new(LintersTable {
            severity: [("ol-prefix".to_string(), RuleSeverity::Error)].into(),
            settings: LintersSettingsTable {
                ol_prefix: MD029OlPrefixTable { style },
                ..Default::default()
            },
        })
    }

    fn owned(wants: &[Want]) -> Vec<Found> {
        wants
            .iter()
            .map(|&(line, expected, actual, style)| (line, expected, actual, style.to_string()))
            .collect()
    }

    fn reports(style: OlPrefixStyle, source: &str) -> Vec<Found> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config(style), source);
        let mut found: Vec<Found> = linter
            .analyze()
            .iter()
            .filter_map(|violation| {
                let (rest, style) = violation.message().split_once("; Style: ")?;
                let (expected, actual) = rest.split_once("; Actual: ")?;
                Some((
                    violation.location().range.start.line + 1,
                    expected
                        .strip_prefix("Ordered list item prefix [Expected: ")?
                        .parse()
                        .ok()?,
                    actual.parse().ok()?,
                    style.trim_end_matches(']').to_string(),
                ))
            })
            .collect();
        // markdownlint sorts a file's reports by line. A nested list is visited after the list that
        // holds it, so without this the two orders differ for nothing but that.
        found.sort();
        found
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "single item at two",
                "2. a\n",
                [
                    &[(1, 1, 2, ONE)],
                    &[(1, 1, 2, ONE)],
                    &[(1, 1, 2, ORDERED)],
                    &[(1, 0, 2, ZERO)],
                ],
            ),
            (
                "two items from two",
                "2. a\n3. b\n",
                [
                    &[(1, 1, 2, ORDERED), (2, 2, 3, ORDERED)],
                    &[(1, 1, 2, ONE), (2, 1, 3, ONE)],
                    &[(1, 1, 2, ORDERED), (2, 2, 3, ORDERED)],
                    &[(1, 0, 2, ZERO), (2, 0, 3, ZERO)],
                ],
            ),
            (
                "incrementing from one",
                "1. a\n2. b\n",
                [
                    &[],
                    &[(2, 1, 2, ONE)],
                    &[],
                    &[(1, 0, 1, ZERO), (2, 0, 2, ZERO)],
                ],
            ),
            (
                "zero based",
                "0. a\n1. b\n",
                [&[], &[(1, 1, 0, ONE)], &[], &[(2, 0, 1, ZERO)]],
            ),
            (
                "zero based with a gap",
                "0. a\n2. b\n",
                [
                    &[(2, 1, 2, ORDERED)],
                    &[(1, 1, 0, ONE), (2, 1, 2, ONE)],
                    &[(2, 1, 2, ORDERED)],
                    &[(2, 0, 2, ZERO)],
                ],
            ),
            (
                "incrementing from three",
                "3. a\n4. b\n",
                [
                    &[(1, 1, 3, ORDERED), (2, 2, 4, ORDERED)],
                    &[(1, 1, 3, ONE), (2, 1, 4, ONE)],
                    &[(1, 1, 3, ORDERED), (2, 2, 4, ORDERED)],
                    &[(1, 0, 3, ZERO), (2, 0, 4, ZERO)],
                ],
            ),
            (
                "all zeros",
                "0. a\n0. b\n",
                [
                    &[(2, 1, 0, ORDERED)],
                    &[(1, 1, 0, ONE), (2, 1, 0, ONE)],
                    &[(2, 1, 0, ORDERED)],
                    &[],
                ],
            ),
            (
                "single item at one",
                "1. a\n",
                [&[], &[], &[], &[(1, 0, 1, ZERO)]],
            ),
            (
                "single item at five",
                "5. a\n",
                [
                    &[(1, 1, 5, ONE)],
                    &[(1, 1, 5, ONE)],
                    &[(1, 1, 5, ORDERED)],
                    &[(1, 0, 5, ZERO)],
                ],
            ),
            (
                "single item at zero",
                "0. a\n",
                [
                    &[(1, 1, 0, ONE)],
                    &[(1, 1, 0, ONE)],
                    &[(1, 1, 0, ORDERED)],
                    &[],
                ],
            ),
            (
                "all ones",
                "1. a\n1. b\n1. c\n",
                [
                    &[],
                    &[],
                    &[(2, 2, 1, ORDERED), (3, 3, 1, ORDERED)],
                    &[(1, 0, 1, ZERO), (2, 0, 1, ZERO), (3, 0, 1, ZERO)],
                ],
            ),
            (
                "odd steps",
                "1. a\n3. b\n5. c\n",
                [
                    &[(2, 2, 3, ORDERED), (3, 3, 5, ORDERED)],
                    &[(2, 1, 3, ONE), (3, 1, 5, ONE)],
                    &[(2, 2, 3, ORDERED), (3, 3, 5, ORDERED)],
                    &[(1, 0, 1, ZERO), (2, 0, 3, ZERO), (3, 0, 5, ZERO)],
                ],
            ),
            (
                "zero based and continuous",
                "0. a\n1. b\n2. c\n",
                [
                    &[],
                    &[(1, 1, 0, ONE), (3, 1, 2, ONE)],
                    &[],
                    &[(2, 0, 1, ZERO), (3, 0, 2, ZERO)],
                ],
            ),
            (
                "parenthesis delimiters",
                "3) a\n5) b\n",
                [
                    &[(1, 1, 3, ORDERED), (2, 2, 5, ORDERED)],
                    &[(1, 1, 3, ONE), (2, 1, 5, ONE)],
                    &[(1, 1, 3, ORDERED), (2, 2, 5, ORDERED)],
                    &[(1, 0, 3, ZERO), (2, 0, 5, ZERO)],
                ],
            ),
            (
                "nested list of ones",
                "1. a\n   1. x\n   1. y\n2. b\n",
                [
                    &[],
                    &[(4, 1, 2, ONE)],
                    &[(3, 2, 1, ORDERED)],
                    &[
                        (1, 0, 1, ZERO),
                        (2, 0, 1, ZERO),
                        (3, 0, 1, ZERO),
                        (4, 0, 2, ZERO),
                    ],
                ],
            ),
            (
                "nested list that increments",
                "1. a\n   1. x\n   2. y\n2. b\n",
                [
                    &[],
                    &[(3, 1, 2, ONE), (4, 1, 2, ONE)],
                    &[],
                    &[
                        (1, 0, 1, ZERO),
                        (2, 0, 1, ZERO),
                        (3, 0, 2, ZERO),
                        (4, 0, 2, ZERO),
                    ],
                ],
            ),
            (
                "nested unordered list",
                "1. a\n   - x\n   - y\n2. b\n",
                [
                    &[],
                    &[(4, 1, 2, ONE)],
                    &[],
                    &[(1, 0, 1, ZERO), (4, 0, 2, ZERO)],
                ],
            ),
            // The blank lines do not split the list: 100 is its third item, so 3 is what it should
            // have shown. Splitting on blank lines and numbering gaps is a tree-sitter-era guess
            // that comrak's own list grouping makes unnecessary.
            (
                "loose with a jump",
                "1. a\n\n2. b\n\n100. c\n",
                [
                    &[(5, 3, 100, ORDERED)],
                    &[(3, 1, 2, ONE), (5, 1, 100, ONE)],
                    &[(5, 3, 100, ORDERED)],
                    &[(1, 0, 1, ZERO), (3, 0, 2, ZERO), (5, 0, 100, ZERO)],
                ],
            ),
            (
                "tight with a jump",
                "1. a\n2. b\n100. c\n",
                [
                    &[(3, 3, 100, ORDERED)],
                    &[(2, 1, 2, ONE), (3, 1, 100, ONE)],
                    &[(3, 3, 100, ORDERED)],
                    &[(1, 0, 1, ZERO), (2, 0, 2, ZERO), (3, 0, 100, ZERO)],
                ],
            ),
            (
                "inside a block quote",
                "> 1. one\n>    cont\n> 2. two\n",
                [
                    &[],
                    &[(3, 1, 2, ONE)],
                    &[],
                    &[(1, 0, 1, ZERO), (3, 0, 2, ZERO)],
                ],
            ),
            // `- b` is its own list, so `2. c` opens another one and a lone item is held to 1.
            (
                "unordered item between",
                "1. a\n- b\n2. c\n",
                [
                    &[(3, 1, 2, ONE)],
                    &[(3, 1, 2, ONE)],
                    &[(3, 1, 2, ORDERED)],
                    &[(1, 0, 1, ZERO), (3, 0, 2, ZERO)],
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

    /// micromark never clears the `interrupt` flag an indented code block sets, and its list
    /// factory then accepts an ordered marker of exactly `1` and nothing else. So `2. b` straight
    /// after an indented code block is paragraph text to markdownlint — comrak reads it as a list,
    /// which is what the CommonMark reference does, and the facade rewrites it back. Blank lines
    /// neither set nor clear the flag; every other kind of preceding block clears it.
    ///
    /// Each expectation is markdownlint-cli2 v0.23.3 output under the style named.
    #[test]
    fn a_list_after_indented_code_is_a_paragraph() {
        let none: &[Want] = &[];
        let cases: &[Case] = &[
            // A paragraph before the code block changes nothing.
            (
                "paragraph then code",
                "x\n\n    code\n\n2. b\n",
                [none, none, none, none],
            ),
            // Both lines fold into one paragraph, so neither is numbered.
            (
                "two items",
                "    code\n\n2. b\n3. c\n",
                [none, none, none, none],
            ),
            // `list.js` also wants a single digit, so `10.` and `01.` are refused too.
            (
                "two digits",
                "    code\n\n10. b\n",
                [none, none, none, none],
            ),
            (
                "zero padded",
                "    code\n\n01. b\n",
                [none, none, none, none],
            ),
            (
                "parenthesis",
                "    code\n\n2) b\n",
                [none, none, none, none],
            ),
            // `1.` may interrupt, so this is a list and its second item is judged normally.
            (
                "starts at one",
                "    code\n\n1. b\n\n2. c\n",
                [
                    none,
                    &[(5, 1, 2, ONE)],
                    none,
                    &[(3, 0, 1, ZERO), (5, 0, 2, ZERO)],
                ],
            ),
            // An unordered list clears the flag, so the `2. b` after it is a one-item ordered list.
            (
                "unordered between",
                "    code\n\n- x\n\n2. b\n",
                [
                    &[(5, 1, 2, ONE)],
                    &[(5, 1, 2, ONE)],
                    &[(5, 1, 2, ORDERED)],
                    &[(5, 0, 2, ZERO)],
                ],
            ),
            // The same inside a block quote, where the code block is the quote's own.
            (
                "in a block quote",
                "> q\n>\n>     code\n>\n> 2. b\n",
                [none, none, none, none],
            ),
        ];

        let mut failures = Vec::new();
        for (name, source, wants) in cases {
            for (index, style) in STYLES.iter().enumerate() {
                let (actual, expected) = (reports(*style, source), owned(wants[index]));
                if actual != expected {
                    failures.push(format!(
                        "{name} [{style:?}]: want {expected:?}, got {actual:?}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}\n", failures.join("\n"));
    }
}
