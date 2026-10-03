use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{Rule, RuleType},
};

/// MD039 - Spaces inside link text
///
/// Reports the whitespace just inside a link's `[` and `]`. An image's label is left alone, and so
/// is a bracket run that never became a link — `[ b ]` with no `[b]` defined is text.
pub(crate) struct MD039Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD039Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch.
    fn walk(&mut self, root: Node) {
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "link" {
                self.check(node);
            }
            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
                    return;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                cursor.goto_parent();
                depth -= 1;
            }
        }
    }

    fn check(&mut self, link: Node) {
        let Some(label) = label_span(link, &self.context.document_content.borrow()) else {
            return;
        };

        let (context, found) = {
            let source = self.context.document_content.borrow();
            let text = &source[label.from..label.to];
            let found = [
                // markdownlint asks `trimStart`/`trimEnd`, which count every whitespace character,
                // but reports the run of *horizontal* whitespace — and when the label starts or ends
                // with a line break there is no such run, so it names the line beside it instead.
                (text.len() != text.trim_start().len()).then(|| {
                    let run = horizontal_run(text, true);
                    Side::Leading.at(self.context.point_at(label.from), run)
                }),
                (text.len() != text.trim_end().len()).then(|| {
                    let run = horizontal_run(text, false);
                    Side::Trailing.at(self.context.point_at(label.to), run)
                }),
            ];
            (ellipsify(&source[label.from - 1..=label.to]), found)
        };

        for side in found.into_iter().flatten() {
            self.violations.push(RuleViolation::new(
                &MD039,
                format!(
                    "{} [Context: \"{}\"]",
                    MD039.description,
                    ellipsified(&context, side)
                ),
                self.context.file_path.clone(),
                range_from_node_range(&crate::ast::NodeRange {
                    start_byte: 0,
                    end_byte: 0,
                    start_point: crate::ast::Point {
                        row: side.row,
                        column: side.column,
                    },
                    end_point: crate::ast::Point {
                        row: side.row,
                        column: side.column + side.width,
                    },
                }),
            ));
        }
    }
}

/// Which end of the label a report is about, and where it lands.
#[derive(Clone, Copy)]
enum Side {
    Leading,
    Trailing,
}

impl Side {
    /// The row and column markdownlint names, and the width of the run there. Without a horizontal
    /// run the report moves one line towards the label's middle and covers nothing.
    fn at(self, end: crate::ast::Point, run: usize) -> Placed {
        let (row, column) = match (self, run) {
            (Self::Leading, 0) => (end.row + 1, 0),
            (Self::Trailing, 0) => (end.row.saturating_sub(1), 0),
            (Self::Leading, _) => (end.row, end.column),
            (Self::Trailing, run) => (end.row, end.column.saturating_sub(run)),
        };
        Placed {
            row,
            column,
            width: run,
            trailing: matches!(self, Self::Trailing),
        }
    }
}

#[derive(Clone, Copy)]
struct Placed {
    row: usize,
    column: usize,
    width: usize,
    trailing: bool,
}

/// The byte range of a link's label — what sits between the `[` the node starts at and its matching
/// `]`. Counted rather than taken from the node's children, because a label of nothing but
/// whitespace has no children and one holding a code span has several.
fn label_span(link: Node, source: &str) -> Option<Label> {
    let bytes = source.as_bytes();
    let from = link.start_byte();
    if bytes.get(from) != Some(&b'[') {
        return None;
    }
    let mut depth = 0usize;
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'[' => {
                depth += 1;
                at += 1;
            }
            b']' => {
                depth -= 1;
                at += 1;
                if depth == 0 {
                    return Some(Label {
                        from: from + 1,
                        to: at - 1,
                    });
                }
            }
            _ => at += 1,
        }
    }
    None
}

struct Label {
    from: usize,
    to: usize,
}

/// How many whitespace characters that are neither `\r` nor `\n` sit at the start (`leading`) or the
/// end of `text` — markdownlint's `[^\S\r\n]`.
fn horizontal_run(text: &str, leading: bool) -> usize {
    let is_horizontal = |ch: char| ch.is_whitespace() && ch != '\n' && ch != '\r';
    let width = |chars: &mut dyn Iterator<Item = char>| {
        chars
            .take_while(|&ch| is_horizontal(ch))
            .map(char::len_utf8)
            .sum()
    };
    if leading {
        width(&mut text.chars())
    } else {
        width(&mut text.chars().rev())
    }
}

/// The label with every whitespace run collapsed to one space, which is what markdownlint puts in
/// the message before [`ellipsified`] shortens it.
fn ellipsify(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// markdownlint's `ellipsify`: over thirty characters, a leading report keeps the start and a
/// trailing one the end.
fn ellipsified(text: &str, side: Placed) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 30 {
        return text.to_string();
    }
    let head = |count: usize| chars.iter().take(count).collect::<String>();
    let tail = |count: usize| chars.iter().skip(chars.len() - count).collect::<String>();
    if side.trailing {
        format!("...{}", tail(30))
    } else {
        format!("{}...", head(30))
    }
}

impl RuleLinter for MD039Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.walk(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD039: Rule = Rule {
    id: "MD039",
    alias: "no-space-in-links",
    tags: &["whitespace", "links"],
    description: "Spaces inside link text",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD039Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-space-in-links", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
            ("line-length", RuleSeverity::Off),
        ])
    }

    /// Every case below is followed by these two definitions, so the reference links among them
    /// resolve and become links rather than text.
    const DEFS: &str = "\n[r]: /u\n[a]: /u\n";

    /// A report: the 1-based line and column of the whitespace run, and the label markdownlint puts
    /// in the message.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);

    /// A case's name, its document without [`DEFS`], and the reports markdownlint makes on it.
    type Case = (&'static str, &'static str, &'static [Report]);

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, context)| (line, column, context.to_string()))
            .collect()
    }

    fn reports(body: &str) -> Vec<Found> {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            test_config(),
            &format!("{body}{DEFS}"),
        );
        linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD039")
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

    /// Every expectation measured against markdownlint-cli2 v0.23.3, which reports the whitespace
    /// run itself — so a label padded both sides is two reports, and one whose padding is a line
    /// break moves to the line beside it.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "leading and trailing",
                "[ link ](url)\n",
                &[(1, 2, "[ link ]"), (1, 7, "[ link ]")],
            ),
            ("no spaces", "[link](url)\n", &[]),
            ("an image is left alone", "![ img ](i)\n", &[]),
            (
                "a full reference",
                "[ a ][r]\n",
                &[(1, 2, "[ a ]"), (1, 4, "[ a ]")],
            ),
            (
                "a collapsed reference",
                "[ a ][]\n",
                &[(1, 2, "[ a ]"), (1, 4, "[ a ]")],
            ),
            (
                "a shortcut reference",
                "[ a ]\n",
                &[(1, 2, "[ a ]"), (1, 4, "[ a ]")],
            ),
            ("an undefined shortcut is not a link", "[ b ]\n", &[]),
            ("an empty label", "[](url)\n", &[]),
            (
                "a label of one space",
                "[ ](url)\n",
                &[(1, 2, "[ ]"), (1, 2, "[ ]")],
            ),
            (
                "nested brackets",
                "[a [b] c ](u)\n",
                &[(1, 9, "[a [b] c ]")],
            ),
            (
                "two spaces each side",
                "[  a  ](u)\n",
                &[(1, 2, "[ a ]"), (1, 5, "[ a ]")],
            ),
            ("a trailing tab", "[a\t](u)\n", &[(1, 3, "[a ]")]),
            (
                "trailing space on the second line",
                "[a\n b ](u)\n",
                &[(2, 3, "[a b ]")],
            ),
            (
                "leading space on the first line",
                "[ a\nb](u)\n",
                &[(1, 2, "[ a b]")],
            ),
            (
                "a trailing space before a line break",
                "[a \n](u)\n",
                &[(1, 1, "[a ]")], // markdownlint names no column here
            ),
            (
                "a trailing space after a line break",
                "[a\n ](u)\n",
                &[(2, 1, "[a ]")],
            ),
            (
                "in a heading",
                "# [ a ](u)\n",
                &[(1, 4, "[ a ]"), (1, 6, "[ a ]")],
            ),
            (
                "in a table cell",
                "| x |\n| - |\n| [ a ](u) |\n",
                &[(3, 4, "[ a ]"), (3, 6, "[ a ]")],
            ),
            ("an autolink has no label", "<https://x.com>\n", &[]),
            (
                "a code span in the label",
                "[ `a` ](u)\n",
                &[(1, 2, "[ `a` ]"), (1, 6, "[ `a` ]")],
            ),
            (
                "two links",
                "[ a ](u) and [ b ](v)\n",
                &[
                    (1, 2, "[ a ]"),
                    (1, 4, "[ a ]"),
                    (1, 15, "[ b ]"),
                    (1, 17, "[ b ]"),
                ],
            ),
            (
                "a label over thirty characters",
                "[aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ](u)\n",
                &[(1, 40, "...aaaaaaaaaaaaaaaaaaaaaaaaaaaa ]")],
            ),
            (
                "in a block quote",
                "> [ a ](u)\n",
                &[(1, 4, "[ a ]"), (1, 6, "[ a ]")],
            ),
            ("a title", "[link](url \"title\")\n", &[]),
            ("an image reference", "![ img ][r]\n", &[]),
            ("an image inside a link", "[![ img ](i)](u)\n", &[]),
            (
                "an image inside a padded label",
                "[ ![i](x) ](u)\n",
                &[(1, 2, "[ ![i](x) ]"), (1, 10, "[ ![i](x) ]")],
            ),
            (
                "escaped brackets in the label",
                "[ \\[ a \\] ](u)\n",
                &[(1, 2, "[ \\[ a \\] ]"), (1, 10, "[ \\[ a \\] ]")],
            ),
            (
                "in a list item",
                "- [ a ](u)\n",
                &[(1, 4, "[ a ]"), (1, 6, "[ a ]")],
            ),
            (
                "on the second line",
                "[a](u)\n[ b ](v)\n",
                &[(2, 2, "[ b ]"), (2, 4, "[ b ]")],
            ),
            (
                "a label of two spaces",
                "[  ](u)\n",
                &[(1, 2, "[ ]"), (1, 2, "[ ]")],
            ),
            (
                "tabs both sides",
                "[\ta\t](u)\n",
                &[(1, 2, "[ a ]"), (1, 4, "[ a ]")],
            ),
        ];

        let failures: Vec<String> = cases
            .iter()
            .filter(|&&(name, body, expected)| {
                let actual = reports(body);
                if actual == owned(expected) {
                    return false;
                }
                println!("{name}: expected {:?}, got {actual:?}", owned(expected));
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

    /// When the whitespace beside a label is a line break there is no horizontal run to point at, so
    /// markdownlint passes no range and markdownlint-cli2 prints the line alone. quickmark always
    /// has a range, and puts it at the start of that line.
    #[test]
    fn a_line_break_inside_the_label_has_no_column() {
        assert_eq!(reports("[a \n](u)\n"), vec![(1, 1, "[a ]".to_string())]);
    }
}
