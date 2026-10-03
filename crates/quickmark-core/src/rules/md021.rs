use crate::ast::Node;
use std::rc::Rc;

use crate::linter::{Context, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

pub(crate) struct MD021Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD021Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Reports each whitespace run inside a closed ATX heading's hashes that is longer than one.
    ///
    /// markdownlint walks the heading token's children from each end to the `atxHeadingSequence`
    /// there and looks at the `whitespace` beside it, so a heading with no text between its hashes
    /// reports the one run twice. The facade has no whitespace nodes, so both runs are measured
    /// against the line: the leading one from the opening hashes to the first non-space, the
    /// trailing one from the last non-space to the closing hashes.
    fn check_heading_spaces(&mut self, node: &Node) {
        if !node.is_closed() {
            return;
        }
        let Some(marker) = node.child(0) else {
            return;
        };
        let kind = marker.kind();
        if !kind.starts_with("atx_h") || !kind.ends_with("_marker") {
            return;
        }

        let row = node.start_position().row;
        let (runs, context) = {
            let (line_start, line_end) = {
                let lines = self.context.lines.borrow();
                let start = self.context.line_start_byte(row);
                (start, start + lines.get(row).map_or(0, |line| line.len()))
            };
            let source = self.context.get_document_content();
            let bytes = source.as_bytes();
            let is_space = |at: usize| matches!(bytes[at], b' ' | b'\t');

            // A heading is closed by the run of hashes at the end of its line, once trailing
            // whitespace is set aside.
            let trailing = (line_start..line_end)
                .rev()
                .take_while(|&at| is_space(at))
                .count();
            let content_end = line_end - trailing;
            let hashes = bytes[line_start..content_end]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'#')
                .count();
            let close_start = content_end - hashes;
            let open_end = marker.end_byte().clamp(line_start, close_start);

            let lead_end = (open_end..close_start)
                .find(|&at| !is_space(at))
                .unwrap_or(close_start);
            let trail_start = (open_end..close_start)
                .rev()
                .find(|&at| !is_space(at))
                .map_or(open_end, |at| at + 1);
            let runs = [
                (line_start, open_end, lead_end),
                (line_start, trail_start, close_start),
            ]
            .into_iter()
            // markdownlint reports the leading run first, and ellipsifies towards the end the
            // report is about.
            .zip([true, false])
            .filter(|((_, from, to), _)| to - from > 1)
            .collect::<Vec<_>>();
            (runs, source[line_start..line_end].trim().to_string())
        };
        for ((line_start, from, to), start) in runs {
            self.report(row, line_start, (from, to), &context, start);
        }
    }

    fn report(
        &mut self,
        row: usize,
        line_start: usize,
        run: (usize, usize),
        context: &str,
        start: bool,
    ) {
        // markdownlint points at the second character of the run and covers all but the first, which
        // is the part its fix deletes.
        self.violations.push(RuleViolation::new(
            &MD021,
            format!(
                "{} [Context: \"{}\"]",
                MD021.description,
                ellipsify(context, start, !start)
            ),
            self.context.file_path.clone(),
            crate::linter::Range {
                start: crate::linter::CharPosition {
                    line: row,
                    character: run.0 - line_start + 1,
                },
                end: crate::linter::CharPosition {
                    line: row,
                    character: run.1 - line_start,
                },
            },
        ));
    }
}

impl RuleLinter for MD021Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "atx_heading" {
            self.check_heading_spaces(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD021: Rule = Rule {
    id: "MD021",
    alias: "no-multiple-space-closed-atx",
    tags: &["headings", "atx_closed", "spaces"],
    description: "Multiple spaces inside hashes on closed atx style heading",
    rule_type: RuleType::Token,
    required_nodes: &["atx_heading"],
    new_linter: |context| Box::new(MD021Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// A report: the 1-based line and column, and the heading markdownlint quotes. The column is the
    /// second character of the run, which is the part its fix deletes.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);
    type Case = (&'static str, &'static str, &'static [Report]);

    fn reports(input: &str) -> Vec<Found> {
        let config = test_config_with_rules(vec![
            ("no-multiple-space-closed-atx", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ]);
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

    /// Every expectation measured against markdownlint-cli2 v0.23.3. A heading with no text between
    /// its hashes reports the one run twice, because markdownlint walks in from each end and both
    /// ends find it.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            ("an open heading with no space", "#x\n", &[]),
            ("an open heading with one space", "# x\n", &[]),
            ("an open heading with two spaces", "#  x\n", &[]),
            ("an open heading with three spaces", "#   x\n", &[]),
            ("a closed heading with no spaces", "#x#\n", &[]),
            ("a closed heading with one space each side", "# x #\n", &[]),
            (
                "a closed heading with two spaces each side",
                "#  x  #\n",
                &[(1, 3, "#  x  #"), (1, 6, "#  x  #")],
            ),
            (
                "a closed heading with two spaces before the hashes",
                "# x  #\n",
                &[(1, 5, "# x  #")],
            ),
            (
                "a closed heading with two spaces after the hashes",
                "#  x #\n",
                &[(1, 3, "#  x #")],
            ),
            ("two hashes and nothing else", "##\n", &[]),
            (
                "two hashes around two spaces",
                "##  ##\n",
                &[(1, 4, "##  ##"), (1, 4, "##  ##")],
            ),
            ("two hashes around one space", "## ##\n", &[]),
            ("four hashes and nothing else", "####\n", &[]),
            ("a heading closed by an escaped hash", "# x \\#\n", &[]),
            ("a hash inside the text", "# a#b #\n", &[]),
            ("an open heading with a tab", "#\tx\n", &[]),
            ("a closed heading with tabs", "# x\t#\n", &[]),
            ("a closed heading followed by spaces", "# x #  \n", &[]),
            ("an indented open heading", "  #  x\n", &[]),
            (
                "an indented closed heading",
                "  #  x  #\n",
                &[(1, 5, "#  x  #"), (1, 8, "#  x  #")],
            ),
            (
                "a closed heading in a fenced code block",
                "```\n#  x  #\n```\n",
                &[],
            ),
            (
                "a closed heading after a fenced code block",
                "```\ncode\n```\n#  x  #\n",
                &[(4, 3, "#  x  #"), (4, 6, "#  x  #")],
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
                &[(1, 3, "#  x  #  y  #"), (1, 12, "#  x  #  y  #")],
            ),
            (
                "a closed heading with no space after the hashes",
                "#x  #\n",
                &[],
            ),
            (
                "a closed heading with no space before the hashes",
                "#  x#\n",
                &[],
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
                &[(1, 3, "#  x  #"), (1, 6, "#  x  #")],
            ),
            ("a tab indented open heading", "\t#  x\n", &[]),
            (
                "a heading whose closing hash is not at the end",
                "#  x  # trailing\n",
                &[],
            ),
            ("a heading with two hashes inside", "# x # x #\n", &[]),
            ("a closed heading around a dash", "#-#\n", &[]),
        ];
        for (name, input, expected) in cases {
            assert_eq!(owned(expected), reports(input), "{name}");
        }
    }
}
