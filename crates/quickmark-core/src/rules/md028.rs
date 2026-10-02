use std::rc::Rc;

use crate::ast::Node;

use crate::linter::{CharPosition, Range, RuleViolation};

use crate::rules::{Context, Rule, RuleLinter, RuleType};

pub(crate) struct MD028Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD028Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Reports the blank rows between one block quote and whatever follows it, when that is another
    /// block quote.
    ///
    /// markdownlint walks the token's siblings and collects blank line endings until it meets
    /// something that is not one, and a block quote is the only thing that turns them into
    /// violations — a paragraph, an indented code block or an HTML block in between ends the run
    /// quietly. Reading that off the tree rather than off lines beginning with `>` is what keeps a
    /// `>` inside a fenced code block from looking like a quote, which is why this rule needs no
    /// list of code block lines to exclude.
    fn check(&mut self, node: Node) {
        let Some(next) = next_sibling(node) else {
            return;
        };
        if next.kind() != "block_quote" {
            return;
        }
        let blanks = {
            let lines = self.context.lines.borrow();
            (last_row(node) + 1..next.start_position().row)
                .filter(|&row| lines.get(row).is_some_and(|line| line.trim().is_empty()))
                .collect::<Vec<_>>()
        };
        for row in blanks {
            self.violations.push(RuleViolation::new(
                &MD028,
                "Blank line inside blockquote".to_string(),
                self.context.file_path.clone(),
                Range {
                    start: CharPosition {
                        line: row,
                        character: 0,
                    },
                    end: CharPosition {
                        line: row,
                        character: self
                            .context
                            .lines
                            .borrow()
                            .get(row)
                            .map_or(0, |line| line.len()),
                    },
                },
            ));
        }
    }
}

/// The node after `node` under the same parent.
fn next_sibling<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let parent = node.parent()?;
    let position = (0..parent.child_count()).find(|&index| {
        parent
            .child(index)
            .is_some_and(|child| child.id() == node.id())
    })?;
    parent.child(position + 1)
}

/// The row a block's last content is on: a block's end swallows its trailing newline, which puts
/// `end_position` on the row after it.
fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 {
        end.row.saturating_sub(1)
    } else {
        end.row
    }
}

impl RuleLinter for MD028Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "block_quote" {
            self.check(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD028: Rule = Rule {
    id: "MD028",
    alias: "no-blanks-blockquote",
    tags: &["blockquote", "whitespace"],
    description: "Blank lines inside blockquotes",
    rule_type: RuleType::Token,
    required_nodes: &["block_quote"],
    new_linter: |context| Box::new(MD028Linter::new(context)),
};

#[cfg(test)]
mod tests {
    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;
    use std::path::PathBuf;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-blanks-blockquote", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ])
    }

    #[test]
    fn test_md028_violation_basic() {
        let input = r#"> First blockquote

> Second blockquote"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This test should fail initially (TDD approach)
        assert!(
            !violations.is_empty(),
            "Should detect blank line inside blockquote"
        );
    }

    #[test]
    fn test_md028_valid_continuous_blockquote() {
        let input = r#"> First line
> Second line"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This should not violate - continuous blockquote
        assert!(
            violations.is_empty(),
            "Should not violate for continuous blockquote"
        );
    }

    #[test]
    fn test_md028_valid_separated_with_content() {
        let input = r#"> First blockquote

Some text here.

> Second blockquote"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This should not violate - properly separated with content
        assert!(
            violations.is_empty(),
            "Should not violate when blockquotes are separated with content"
        );
    }

    #[test]
    fn test_md028_valid_continuous_with_blank_line_marker() {
        let input = r#"> First line
>
> Second line"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This should not violate - blank line with blockquote marker
        assert!(
            violations.is_empty(),
            "Should not violate when blank line has blockquote marker"
        );
    }

    #[test]
    fn test_md028_violation_multiple_blank_lines() {
        let input = r#"> First blockquote


> Second blockquote"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This should violate - multiple blank lines between blockquotes
        assert!(
            !violations.is_empty(),
            "Should detect multiple blank lines inside blockquote"
        );
    }

    #[test]
    fn test_md028_violation_nested_blockquotes() {
        let input = r#"> First level
> > Second level

> > Another second level"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This should violate - blank line in nested blockquotes
        assert!(
            !violations.is_empty(),
            "Should detect blank lines in nested blockquotes"
        );
    }

    /// The lines markdownlint reports, 1-based: every blank one between the two quotes.
    type Line = usize;

    fn lines(source: &str) -> Vec<Line> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .map(|violation| violation.location().range.start.line + 1)
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD028's defaults.
    ///
    /// What sits between the quotes decides, not what the lines look like: a fenced code block
    /// holding a `>` is not a quote, and a quote holding a fenced code block still is one.
    const CASES: &[(&str, &[Line])] = &[
        ("> a\n\n> b\n", &[2]),
        ("> a\n\n\n> b\n", &[2, 3]),
        ("> a\n\nb\n", &[]),
        ("> a\n\ntext\n\n> b\n", &[]),
        ("text\n\n> a\n\n> b\n", &[4]),
        ("> a\n> c\n\n> b\n", &[3]),
        ("> a\n\n> b\n\n> c\n", &[2, 4]),
        ("- x\n\n> a\n\n> b\n", &[4]),
        ("> a\n\n    code\n\n> b\n", &[]),
        ("> a\n\n> b\n\ntext\n", &[2]),
        ("> ```\n> x\n> ```\n\n> b\n", &[4]),
        ("> a\n\n<div>\n</div>\n\n> b\n", &[]),
        ("> > a\n>\n> > b\n", &[]),
        ("> > a\n\n> > b\n", &[2]),
        ("- > a\n  > b\n", &[]),
        ("- > a\n\n  > b\n", &[2]),
        ("> a\r\n\r\n> b\r\n", &[2]),
        ("> a\n\n\n\n> b\n", &[2, 3, 4]),
        ("> a\n# h\n\n> b\n", &[]),
        ("> a\n\n> b\n> c\n\n> d\n", &[2, 5]),
        ("> a\n\n> b\n\n> c\n\n> d\n", &[2, 4, 6]),
        ("```\n> a\n```\n\n> b\n", &[]),
        ("> a\n\n> b\ntext\n", &[2]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            let mut found = lines(source);
            found.sort_unstable();
            assert_eq!(expected, found.as_slice(), "source {source:?}");
        }
    }
}
