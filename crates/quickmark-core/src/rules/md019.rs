use crate::ast::Node;
use std::rc::Rc;

use crate::linter::{Context, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

pub(crate) struct MD019Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD019Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Reports the run of whitespace after an ATX heading's hashes when it is longer than one.
    ///
    /// A closed heading belongs to MD021 and not here: markdownlint splits the two on how many
    /// `atxHeadingSequence` tokens micromark emitted, so `#  H  #` gets MD021 twice and no MD019.
    /// The run is measured against the line rather than the heading's text node, because a heading
    /// with nothing after its hashes has no text node and `##  ` is still reported.
    fn check_heading_spaces(&mut self, node: &Node) {
        if node.is_closed() {
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
        let (line_start, line_end) = {
            let lines = self.context.lines.borrow();
            let start = self.context.line_start_byte(row);
            (start, start + lines.get(row).map_or(0, |line| line.len()))
        };
        let source = self.context.get_document_content();
        let from = marker.end_byte().max(line_start).min(line_end);
        let to = (from..line_end)
            .find(|&at| !matches!(source.as_bytes()[at], b' ' | b'\t'))
            .unwrap_or(line_end);
        if to - from <= 1 {
            return;
        }

        // markdownlint reports from the second character of the run to its end, which is the part
        // its fix deletes, and quotes the heading line trimmed — the start of it, because that is
        // the end the report is about.
        let start = from - line_start + 1;
        let context = ellipsify(source[line_start..line_end].trim(), true, false);
        self.violations.push(RuleViolation::new(
            &MD019,
            format!("{} [Context: \"{context}\"]", MD019.description),
            self.context.file_path.clone(),
            crate::linter::Range {
                start: crate::linter::CharPosition {
                    line: row,
                    character: start,
                },
                end: crate::linter::CharPosition {
                    line: row,
                    character: to - line_start,
                },
            },
        ));
    }
}

impl RuleLinter for MD019Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "atx_heading" {
            self.check_heading_spaces(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD019: Rule = Rule {
    id: "MD019",
    alias: "no-multiple-space-atx",
    tags: &["headings", "atx", "spaces"],
    description: "Multiple spaces after hash on atx style heading",
    rule_type: RuleType::Token,
    required_nodes: &["atx_heading"],
    new_linter: |context| Box::new(MD019Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// `(line, column)` of one report, both 1-based, which is markdownlint's `errorRange` start: the
    /// second character of the run, since that is the part its fix deletes.
    type Position = (usize, usize);

    fn positions(source: &str) -> Vec<Position> {
        let config = test_config_with_rules(vec![
            ("no-multiple-space-atx", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ]);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                (range.start.line + 1, range.start.character + 1)
            })
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD019's defaults.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, and every case below has its
    /// multi-byte characters after the reported run, so the two agree throughout.
    const CASES: &[(&str, &[Position])] = &[
        // A closed heading is MD021's, so it is quiet here however wide the gap is.
        ("#  About  #\n", &[]),
        ("# About  #\n", &[]),
        ("##  ##\n", &[]),
        ("#  #\n", &[]),
        ("##  Heading  ##  \n", &[]),
        ("#\t\tTab  #\n", &[]),
        ("#  About  ##\n", &[]),
        ("#  a  #  b  #\n", &[]),
        ("###   x   ###\n", &[]),
        ("   #  About  #\n", &[]),
        ("> #  About  #\n", &[]),
        ("- #  About  #\n", &[]),
        ("######  six  ######\n", &[]),
        ("#  \u{e9}  #\n", &[]),
        ("# H #\n", &[]),
        ("#  a #  \n", &[]),
        // What keeps a heading open: no trailing hashes at all, hashes that are content, and hashes
        // the parser never made a heading out of.
        ("#  About\n", &[(1, 3)]),
        ("#  foo#\n", &[(1, 3)]),
        ("#  About \\#\n", &[(1, 3)]),
        ("#  x  \\#\n", &[(1, 3)]),
        ("#  About  # trailing\n", &[(1, 3)]),
        ("#  About #\u{fe0f}\u{20e3}\n", &[(1, 3)]),
        ("#\u{fe0f}\u{20e3}  keycap\n", &[]),
        ("#######  seven  #######\n", &[]),
        ("#Heading with no space\n", &[]),
        ("Setext Heading\n==============\n", &[]),
        ("Setext Heading\n--------------\n", &[]),
        // Tabs count, a run of one does not, and a heading with no text still has a run.
        ("##\t\tHeading with tabs\n", &[(1, 4)]),
        ("###  \tHeading with space and tab\n", &[(1, 5)]),
        ("####   Heading with multiple spaces\n", &[(1, 6)]),
        ("##  ATX heading with multiple spaces\n", &[(1, 4)]),
        ("#  H  \t\n", &[(1, 3)]),
        ("#\t H\n", &[(1, 3)]),
        ("#  \u{e9}\n", &[(1, 3)]),
        ("  ##  x\n", &[(1, 6)]),
        ("> ##  y\n", &[(1, 6)]),
        ("- ##  z\n", &[(1, 6)]),
        ("##  \n", &[(1, 4)]),
        ("# \n", &[]),
        ("#\n", &[]),
        ("# Heading 1\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, positions(source).as_slice(), "source {source:?}");
        }
    }

    /// markdownlint quotes the heading line trimmed, ellipsified towards its start because that is
    /// the end the report is about.
    #[test]
    fn a_report_quotes_the_heading() {
        let config = test_config_with_rules(vec![
            ("no-multiple-space-atx", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ]);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, "  #  x  \n");
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert_eq!(
            "Multiple spaces after hash on atx style heading [Context: \"#  x\"]",
            violations[0].message()
        );
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 4, 0, 5),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }

    #[test]
    fn one_report_per_heading() {
        let source = "##  Heading 2\n###   Heading 3\n####    Heading 4\n";
        assert_eq!(vec![(1, 4), (2, 5), (3, 6)], positions(source));
    }
}
