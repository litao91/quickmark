use crate::ast::Node;
use std::rc::Rc;

use crate::linter::{CharPosition, Context, Range, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

pub(crate) struct MD014Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD014Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Checks one code block.
    ///
    /// markdownlint reports every line of a block whose lines are *all* shell commands, because a
    /// line that is not one is the output the rule exists to ask for. Its column is the `$` itself,
    /// which is where micromark's `codeFlowValue` token starts plus whatever whitespace the code text
    /// begins with — so it is the line's first non-whitespace character past the container prefix,
    /// and a block quoted or listed needs that prefix walked before it can be found.
    fn check(&mut self, node: Node) {
        let cap = node.start_position().column;
        let first = node.start_position().row;
        let last = last_row(node);
        let fenced = node.kind() == "fenced_code_block";

        let mut found: Vec<(usize, usize, usize)> = Vec::new();
        let mut code_lines = 0usize;
        {
            let lines = self.context.lines.borrow();
            // The block starts at its container prefix, not at its fence, so an indented one has
            // whitespace between the two.
            let opener = fenced.then(|| {
                lines
                    .get(first)
                    .and_then(|line| line[cap.min(line.len())..].trim_start().as_bytes().first())
                    .copied()
                    .unwrap_or(b'`')
            });
            // A fenced block's own first row is its opening fence; an indented block has none.
            let mut row = usize::from(fenced) + first;
            while row <= last {
                let Some(line) = lines.get(row) else {
                    break;
                };
                let (prefix_end, text_start) = line_columns(line, cap);
                let is_fence =
                    opener.is_some_and(|opener| is_closing_fence(&line[text_start..], opener));
                if is_fence {
                    break;
                }
                // micromark emits no `codeFlowValue` for a line the prefix consumed entirely, and
                // markdownlint's "every line" test counts only the ones it did emit — which is why a
                // blank line between two commands does not excuse them and a whitespace-only one
                // does.
                if prefix_end < line.len() {
                    code_lines += 1;
                    let Some(to) = dollar_run(line, text_start) else {
                        return;
                    };
                    found.push((row, text_start, to));
                }
                row += 1;
            }
        }

        if found.len() == code_lines && code_lines > 0 {
            let lines = self.context.lines.borrow();
            for (row, from, to) in found {
                // markdownlint quotes the code line from where its text starts, trailing whitespace
                // and all.
                let context = lines
                    .get(row)
                    .map_or("", |line| &line[from.min(line.len())..]);
                self.violations.push(RuleViolation::new(
                    &MD014,
                    format!(
                        "{} [Context: \"{}\"]",
                        MD014.description,
                        ellipsify(context, false, false)
                    ),
                    self.context.file_path.clone(),
                    Range {
                        start: CharPosition {
                            line: row,
                            character: from,
                        },
                        end: CharPosition {
                            line: row,
                            character: to,
                        },
                    },
                ));
            }
        }
    }
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

/// Where a code block line's container prefix ends, and where its code text begins, both as byte
/// columns.
///
/// `cap` is the column the block itself starts at, which is how wide the prefix it was opened behind
/// can be. Walking no further than that is what keeps a `>` or a `-` inside a top-level fenced block
/// from being read as a container: there the cap is zero and the line is all code.
fn line_columns(line: &str, cap: usize) -> (usize, usize) {
    let bytes = line.as_bytes();
    let limit = cap.min(bytes.len());
    let mut column = 0;
    while column < limit {
        match bytes[column] {
            b' ' | b'\t' => column += 1,
            b'>' => {
                column += 1;
                if column < limit && bytes[column] == b' ' {
                    column += 1;
                }
            }
            b'-' | b'*' | b'+' => {
                column += 1;
                while column < limit && bytes[column] == b' ' {
                    column += 1;
                }
            }
            b'0'..=b'9' => {
                let mut at = column;
                while at < limit && bytes[at].is_ascii_digit() {
                    at += 1;
                }
                if !matches!(bytes.get(at), Some(b'.') | Some(b')')) {
                    break;
                }
                column = at + 1;
                while column < limit && bytes[column] == b' ' {
                    column += 1;
                }
            }
            _ => break,
        }
    }
    let prefix_end = column;
    while column < bytes.len() && matches!(bytes[column], b' ' | b'\t') {
        column += 1;
    }
    (prefix_end, column)
}

/// The end of the `$` and the whitespace after it, or `None` when the line is not a shell command.
/// markdownlint's `/^(\s*)(\$\s+)/` over the code text: the dollar needs whitespace after it, and
/// everything before it has to be whitespace, which is how [`line_columns`] left it.
fn dollar_run(line: &str, text_start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    if bytes.get(text_start) != Some(&b'$') {
        return None;
    }
    let mut end = text_start + 1;
    while matches!(bytes.get(end), Some(b' ') | Some(b'\t')) {
        end += 1;
    }
    (end > text_start + 1).then_some(end)
}

/// Whether a fenced block's line closes it: a run of at least three of the opening character and
/// then nothing but whitespace.
fn is_closing_fence(text: &str, opener: u8) -> bool {
    let bytes = text.as_bytes();
    let run = bytes.iter().take_while(|&&byte| byte == opener).count();
    run >= 3 && bytes[run..].iter().all(|byte| matches!(byte, b' ' | b'\t'))
}

impl RuleLinter for MD014Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "fenced_code_block" | "indented_code_block" => self.check(*node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD014: Rule = Rule {
    id: "MD014",
    alias: "commands-show-output",
    tags: &["code"],
    description: "Dollar signs used before commands without showing output",
    rule_type: RuleType::Token,
    required_nodes: &["fenced_code_block", "indented_code_block"],
    new_linter: |context| Box::new(MD014Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![
                ("commands-show-output", RuleSeverity::Error),
                ("heading-style", RuleSeverity::Off),
                ("heading-increment", RuleSeverity::Off),
            ],
            Default::default(),
        )
    }

    #[test]
    fn test_violation_all_lines_with_dollar_signs() {
        let config = test_config();

        let input = "```bash
$ git status
$ ls -la
$ pwd
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
        assert!(violations[0].message().contains("Dollar signs"));
    }

    #[test]
    fn test_no_violation_with_command_output() {
        let config = test_config();

        let input = "```bash
$ git status
On branch main
nothing to commit

$ ls -la
total 8
drwxr-xr-x 2 user user 4096 Jan 1 00:00 .
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_no_violation_no_dollar_signs() {
        let config = test_config();

        let input = "```bash
git status
ls -la
pwd
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_violation_indented_code_block() {
        let config = test_config();

        let input = "Some text:

    $ git status
    $ ls -la
    $ pwd

More text.";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
        assert!(violations[0].message().contains("Dollar signs"));
    }

    #[test]
    fn test_no_violation_mixed_dollar_signs() {
        let config = test_config();

        let input = "```bash
$ git status
ls -la
$ pwd
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_violation_with_whitespace_before_dollar() {
        let config = test_config();

        let input = "```bash
  $ git status
  $ ls -la
  $ pwd
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
        assert!(violations[0].message().contains("Dollar signs"));
    }

    #[test]
    fn test_no_violation_empty_code_block() {
        let config = test_config();

        let input = "```bash
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_no_violation_blank_lines_only() {
        let config = test_config();

        let input = "```bash



```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_violation_with_blank_lines_between_commands() {
        let config = test_config();

        let input = "```bash
$ git status

$ ls -la

$ pwd
```";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(3, violations.len());
        assert!(violations[0].message().contains("Dollar signs"));
    }

    /// `(line, column)` of one reported command, both 1-based. The column is the `$` itself, which
    /// is markdownlint's `errorRange` start.
    type Position = (usize, usize);

    fn positions(source: &str) -> Vec<Position> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
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
    /// MD014's defaults.
    const CASES: &[(&str, &[Position])] = &[
        (">     $ systemctl --version\n", &[(1, 7)]),
        (">     $ a\n>     out\n", &[]),
        ("> ```\n> $ b\n> ```\n", &[(2, 3)]),
        ("    $ c\n", &[(1, 5)]),
        ("```\n$ d\n```\n", &[(2, 1)]),
        (">     $ e\n\n>     $ f\n", &[(1, 7), (3, 7)]),
        ("-     $ g\n", &[(1, 7)]),
        ("  >     $ h\n", &[(1, 9)]),
        (">     $ i\n>     $ j\n", &[(1, 7), (2, 7)]),
        ("```\n$ k\nout\n```\n", &[]),
        ("> ```\n> $ l\n> out\n> ```\n", &[]),
        (">     $ m\n>\n>     text\n", &[]),
        ("```\n$ a\n\n$ b\n```\n", &[(2, 1), (4, 1)]),
        ("```\n$ a\n   \n$ b\n```\n", &[]),
        ("```\n$a\n```\n", &[]),
        ("```\n$\n```\n", &[]),
        ("```\n$  a\n```\n", &[(2, 1)]),
        ("```\n\t$ a\n```\n", &[(2, 2)]),
        ("```\n$ a\n```\n```\n$ b\n```\n", &[(2, 1), (5, 1)]),
        ("```\n\n```\n", &[]),
        ("  ```\n$ x\n  ```\n", &[(2, 1)]),
        ("   ```\n   $ y\n   ```\n", &[(2, 4)]),
        ("```\n> $ not a quote\n```\n", &[]),
        ("> ```\n> $ q\n> ```\n", &[(2, 3)]),
        ("> ```\n>   $ r\n> ```\n", &[(2, 5)]),
        ("-   ```\n    $ s\n    ```\n", &[(2, 5)]),
        ("```\n$ a\n```\ntext\n", &[(2, 1)]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, positions(source).as_slice(), "source {source:?}");
        }
    }
}
