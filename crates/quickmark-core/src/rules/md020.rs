use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;
use std::rc::Rc;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

static CLOSED_ATX_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(#+)([ \t]*)([^# \t\\]|[^# \t][^#]*?[^# \t\\])([ \t]*)((?:\\#)?)(#+)(\s*)$")
        .expect("Invalid regex for MD020")
});

/// MD020 - No space inside hashes on closed atx style heading
///
/// Line-based, like markdownlint's: the pattern is what decides, and the tree only says which lines
/// to leave alone.
pub(crate) struct MD020Linter {
    context: Rc<Context>,
    /// The 0-based rows markdownlint skips, because a code or HTML block covers them.
    ignored_rows: HashSet<usize>,
}

impl MD020Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            ignored_rows: HashSet::new(),
        }
    }

    /// The rows a block covers. Its end swallows the trailing newline, so the last byte it claims
    /// belongs to the row before the one `end_position` names.
    fn cover(&mut self, node: Node) {
        let from = node.start_position().row;
        let to = self.context.point_at(node.end_byte().saturating_sub(1)).row;
        self.ignored_rows.extend(from..=to);
    }

    fn analyze(&self) -> Vec<RuleViolation> {
        let lines = self.context.lines.borrow();
        lines
            .iter()
            .enumerate()
            .filter(|(row, _)| !self.ignored_rows.contains(row))
            .filter_map(|(row, line)| self.check_line(line, row))
            .collect()
    }

    fn check_line(&self, line: &str, row: usize) -> Option<RuleViolation> {
        let captures = CLOSED_ATX_REGEX.captures(line)?;
        let group = |index: usize| captures.get(index).map_or("", |group| group.as_str());
        let (left_hash, right_hash) = (group(1), group(6));
        let left = group(2).is_empty();
        // An escaped hash on the right is not a space either.
        let right = group(4).is_empty() || !group(5).is_empty();
        if !left && !right {
            return None;
        }
        // markdownlint points at whichever side is missing its space, one column before the closing
        // hashes, and the left side wins when both are.
        let (column, width) = if left {
            (0, left_hash.len() + 1)
        } else {
            (
                line.len() - group(7).len() - right_hash.len() - 1,
                right_hash.len() + 1,
            )
        };
        Some(RuleViolation::new(
            &MD020,
            format!(
                "{} [Context: \"{}\"]",
                MD020.description,
                ellipsify(line.trim(), left, right)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: self.context.line_start_byte(row) + column,
                end_byte: self.context.line_start_byte(row) + column + width,
                start_point: crate::ast::Point { row, column },
                end_point: crate::ast::Point {
                    row,
                    column: column + width,
                },
            }),
        ))
    }
}

impl RuleLinter for MD020Linter {
    fn feed(&mut self, node: &Node) {
        if matches!(
            node.kind(),
            "fenced_code_block" | "indented_code_block" | "html_block"
        ) {
            self.cover(*node);
        }
    }

    /// The scan waits for `finalize` so that every block has been seen, however the document orders
    /// them.
    fn finalize(&mut self) -> Vec<RuleViolation> {
        self.analyze()
    }
}

pub const MD020: Rule = Rule {
    id: "MD020",
    aliases: &["no-missing-space-closed-atx"],
    tags: &["headings", "atx_closed", "spaces"],
    description: "No space inside hashes on closed atx style heading",
    rule_type: RuleType::Line,
    required_nodes: &["fenced_code_block", "indented_code_block", "html_block"],
    new_linter: |context| Box::new(MD020Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// A report: the 1-based line and column, and the heading markdownlint quotes. The column is the
    /// left hash run when that side is missing its space, and the character before the right hash run
    /// otherwise.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);
    type Case = (&'static str, &'static str, &'static [Report]);

    fn reports(input: &str) -> Vec<Found> {
        let config =
            test_config_with_rules(vec![("no-missing-space-closed-atx", RuleSeverity::Error)]);
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
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    context.to_string(),
                )
            })
            .collect()
    }

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, context)| (line, column, context.to_string()))
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3. This rule reads lines, not the
    /// tree, so a heading micromark would not call closed — `#  x#`, whose hashes are not preceded
    /// by a space — is still one as far as it is concerned.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            ("an open heading with no space", "#x\n", &[]),
            ("an open heading with one space", "# x\n", &[]),
            ("an open heading with two spaces", "#  x\n", &[]),
            ("an open heading with three spaces", "#   x\n", &[]),
            ("a closed heading with no spaces", "#x#\n", &[(1, 1, "#x#")]),
            ("a closed heading with one space each side", "# x #\n", &[]),
            (
                "a closed heading with two spaces each side",
                "#  x  #\n",
                &[],
            ),
            (
                "a closed heading with two spaces before the hashes",
                "# x  #\n",
                &[],
            ),
            (
                "a closed heading with two spaces after the hashes",
                "#  x #\n",
                &[],
            ),
            ("two hashes and nothing else", "##\n", &[]),
            ("two hashes around two spaces", "##  ##\n", &[]),
            ("two hashes around one space", "## ##\n", &[]),
            ("four hashes and nothing else", "####\n", &[]),
            ("a heading closed by an escaped hash", "# x \\#\n", &[]),
            ("a hash inside the text", "# a#b #\n", &[]),
            ("an open heading with a tab", "#\tx\n", &[]),
            ("a closed heading with tabs", "# x\t#\n", &[]),
            ("a closed heading followed by spaces", "# x #  \n", &[]),
            ("an indented open heading", "  #  x\n", &[]),
            ("an indented closed heading", "  #  x  #\n", &[]),
            (
                "a closed heading in a fenced code block",
                "```\n#  x  #\n```\n",
                &[],
            ),
            (
                "a closed heading after a fenced code block",
                "```\ncode\n```\n#  x  #\n",
                &[],
            ),
            ("a closed heading in indented code", "    #  x  #\n", &[]),
            ("a setext heading", "Setext\n======\n", &[]),
            (
                "a setext heading with trailing spaces",
                "Setext  \n--------\n",
                &[],
            ),
            ("seven hashes", "#######  x\n", &[]),
            ("a heading closed by more hashes", "## x ###\n", &[]),
            ("a heading closed by fewer hashes", "### x ##\n", &[]),
            (
                "a closed heading with a hash in the text",
                "#  x  #  y  #\n",
                &[],
            ),
            (
                "a closed heading with no space after the hashes",
                "#x  #\n",
                &[(1, 1, "#x  #")],
            ),
            (
                "a closed heading with no space before the hashes",
                "#  x#\n",
                &[(1, 4, "#  x#")],
            ),
            (
                "a closed heading after an html block",
                "<div>\nx\n</div>\n#  x  #\n",
                &[],
            ),
            ("a heading with one space and no text", "# \n", &[]),
            ("a heading with two spaces and no text", "#  \n", &[]),
            (
                "a closed heading and an open one",
                "#  x  #\n\n##  y\n",
                &[],
            ),
            ("a tab indented open heading", "\t#  x\n", &[]),
            (
                "a heading whose closing hash is not at the end",
                "#  x  # trailing\n",
                &[],
            ),
            ("a heading with two hashes inside", "# x # x #\n", &[]),
            ("a closed heading around a dash", "#-#\n", &[(1, 1, "#-#")]),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input), "{name}");
        }
    }
}
