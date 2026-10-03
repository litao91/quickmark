use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

/// `#` followed by U+FE0F U+20E3, which renders as an emoji rather than a heading.
const KEYCAP: &str = "#\u{fe0f}\u{20e3}";

pub(crate) struct MD018Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    /// 1-based lines inside a code or HTML block, where a `#` is literal text.
    ignored: HashSet<usize>,
}

impl MD018Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
            ignored: HashSet::new(),
        }
    }

    fn scan(&mut self) {
        let lines = self.context.lines.borrow();
        for (index, line) in lines.iter().enumerate() {
            if self.ignored.contains(&(index + 1)) {
                continue;
            }
            let Some(hashes) = missing_space(line) else {
                continue;
            };
            self.violations.push(RuleViolation::new(
                &MD018,
                MD018.description.to_string(),
                self.context.file_path.clone(),
                range_from_node_range(&crate::ast::NodeRange {
                    start_byte: 0,
                    end_byte: 0,
                    start_point: crate::ast::Point {
                        row: index,
                        column: 0,
                    },
                    end_point: crate::ast::Point {
                        row: index,
                        column: hashes + 1,
                    },
                }),
            ));
        }
    }
}

/// How many `#` a line opens with when it is a heading missing its space, or `None` when the line
/// is fine. markdownlint's own test is `/^#+[^# \t]/` against the raw line, so indentation puts a
/// `#` out of reach — `      #56219431` inside a list item is text, not a heading — and
/// `!/#\s*$/` leaves alone a line that ends in a `#` however it got there.
fn missing_space(line: &str) -> Option<usize> {
    if line.starts_with(KEYCAP) {
        return None;
    }
    let bytes = line.as_bytes();
    let hashes = bytes.iter().take_while(|&&byte| byte == b'#').count();
    if hashes == 0 {
        return None;
    }
    match bytes.get(hashes) {
        Some(byte) if !matches!(byte, b'#' | b' ' | b'\t') => {}
        _ => return None,
    }
    (!line.trim_end_matches(char::is_whitespace).ends_with('#')).then_some(hashes)
}

impl RuleLinter for MD018Linter {
    fn feed(&mut self, node: &Node) {
        if !matches!(
            node.kind(),
            "fenced_code_block" | "indented_code_block" | "html_block"
        ) {
            return;
        }
        // A block's end swallows its trailing newline, which puts `end_position` on the row after
        // it — unless the document has no final newline, and then it names the last row itself.
        let end = node.end_position();
        let last = if end.column == 0 {
            end.row
        } else {
            end.row + 1
        };
        self.ignored.extend((node.start_position().row + 1)..=last);
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        self.scan();
        std::mem::take(&mut self.violations)
    }
}

pub const MD018: Rule = Rule {
    id: "MD018",
    alias: "no-missing-space-atx",
    tags: &["atx", "headings", "spaces"],
    description: "No space after hash on atx style heading",
    rule_type: RuleType::Line,
    required_nodes: &[],
    new_linter: |context| Box::new(MD018Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-missing-space-atx", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
        ])
    }

    /// The 1-based lines MD018 reports on.
    fn lines(source: &str) -> Vec<usize> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD018")
            .map(|violation| violation.location().range.start.line + 1)
            .collect()
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[(&str, &str, &[usize])] = &[
            ("no space", "#Heading\n", &[1]),
            ("a space", "# Heading\n", &[]),
            ("two hashes", "##Heading\n", &[1]),
            ("hashes only", "###\n", &[]),
            ("two hashes only", "##\n", &[]),
            ("one hash only", "#\n", &[]),
            ("hash and a space", "# \n", &[]),
            ("hash and a tab", "#\tHeading\n", &[]),
            ("keycap emoji", "#\u{fe0f}\u{20e3}Heading\n", &[]),
            ("keycap emoji alone", "#\u{fe0f}\u{20e3}\n", &[]),
            ("keycap emoji with text", "#\u{fe0f}\u{20e3} This should not trigger\n", &[]),
            ("a single letter", "#a\n", &[1]),
            ("non-ascii text", "#\u{e9}Heading\n", &[1]),
            ("six hashes", "######Heading\n", &[1]),
            ("seven hashes", "#######Heading\n", &[1]),
            ("trailing spaces", "#Heading  \n", &[1]),
            ("ends in a hash", "#Heading#\n", &[]),
            ("ends in a spaced hash", "#Heading #\n", &[]),
            ("ends in an escaped hash", "#Heading \\#\n", &[]),
            ("a hash in the middle", "#Heading # trailing\n", &[1]),
            ("a proper closed heading", "# Heading #\n", &[]),
            ("crlf", "#Heading\r\n", &[1]),
            ("after text", "text\n#Heading\n", &[2]),
            ("two in a row", "#1\n#2\n", &[1, 2]),
            ("several mixed", "#Heading 1\n##Heading 2\n### Proper\n####Heading 4\n", &[1, 2, 4]),
            // Indentation puts the `#` out of the line's start, so it is text rather than a heading.
            ("two spaces", "  #Heading\n", &[]),
            ("six spaces", "      #Heading\n", &[]),
            ("four spaces is code", "    #Heading\n", &[]),
            ("four spaces after text", "text\n\n    #Heading\n", &[]),
            ("a tab", "\t#Heading\n", &[]),
            ("in a list item", "- #Heading\n", &[]),
            ("in a list item's continuation", "- item\n\n      #Heading\n", &[]),
            ("in a block quote", "> #Heading\n", &[]),
            ("in a table cell", "| a |\n| - |\n| #Heading |\n", &[]),
            ("not at the start", "Some text #NotAHeading\n", &[]),
            ("in a code span", "`#Heading`\n", &[]),
            ("in a fence", "```\n#Heading\n```\n", &[]),
            ("in a fence, no final newline", "```\n#Heading\n```", &[]),
            ("in a tilde fence", "~~~\n#Heading\n~~~\n", &[]),
            // The block ends where it ends: the line after a closing fence is scanned again.
            ("after a fence", "```\n#Heading\n```\n#Next\n", &[4]),
            ("after a tilde fence", "~~~\n#Heading\n~~~\n#Next\n", &[4]),
            ("before a fence", "text\n\n#Heading\n\n```\n#In\n```\n", &[3]),
            ("in an html block", "<div>\n#Heading\n</div>\n", &[]),
            ("in an html comment", "<!--\n#Heading\n-->\n", &[]),
            // The shape the vault comparison turned up: a `#` that opens a merge-conflict subject
            // six columns into a list item's continuation line.
            (
                "conflict subject in a task list",
                "- [x] CONFLICT (modify/delete): run.sh deleted in HEAD (to\n      #56219431：[adb-pixiu] pixiu). Version c46\n      of run.sh left in tree.\n- [x] next\n",
                &[],
            ),
        ];

        let failures: Vec<String> = cases
            .iter()
            .filter(|&&(name, source, expected)| {
                let actual = lines(source);
                if actual == expected {
                    return false;
                }
                println!("{name}: expected {expected:?}, got {actual:?}");
                true
            })
            .map(|&(name, _, _)| name.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} cases disagree with markdownlint: {failures:?}",
            failures.len(),
            cases.len()
        );
    }

    #[test]
    fn a_report_covers_the_hashes() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            test_config(),
            "##Heading\n",
        );
        let range = linter
            .analyze()
            .iter()
            .find(|violation| violation.rule().id == "MD018")
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            })
            .expect("one violation");
        // markdownlint's range is `[1, hashCount + 1]`: the hashes and the character after them.
        assert_eq!(range, (0, 0, 0, 3));
    }
}
