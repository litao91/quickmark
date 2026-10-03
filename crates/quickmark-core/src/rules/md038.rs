use std::rc::Rc;

use crate::ast::Node;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{ellipsify, Rule, RuleType};

/// One whitespace run markdownlint reports: where it is, the code span it is in, and which end of
/// that span it is at — the two decide how the quoted span is shortened.
struct Run {
    span: (usize, usize),
    run: (usize, usize),
    leading: bool,
}

pub(crate) struct MD038Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD038Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// The whitespace runs markdownlint reports in one inline subtree, as absolute byte ranges.
    fn runs(&self, root: Node) -> Vec<Run> {
        let source = self.context.get_document_content();
        let mut runs = Vec::new();
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "code_span" {
                let span = (node.start_byte(), node.end_byte());
                runs.extend(
                    padded_runs(&source[span.0..span.1], span.0)
                        .into_iter()
                        .map(|(run, leading)| Run { span, run, leading }),
                );
            }

            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
                    return runs;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                cursor.goto_parent();
                depth -= 1;
            }
        }
    }

    fn violation(&self, found: &Run, source: &str) -> RuleViolation {
        let (start, end) = found.run;
        // markdownlint quotes the whole code span, backticks and all, keeping the end the report is
        // about when it has to shorten it.
        let context = ellipsify(
            &source[found.span.0..found.span.1],
            found.leading,
            !found.leading,
        );
        RuleViolation::new(
            &MD038,
            format!("{} [Context: \"{context}\"]", MD038.description),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: start,
                end_byte: end,
                start_point: self.context.point_at(start),
                end_point: self.context.point_at(end),
            }),
        )
    }
}

/// The whitespace markdownlint reports inside one code span's raw text, as absolute byte ranges.
///
/// It reads micromark's `codeText` token, which is this node, and the `codeTextPadding` and
/// `codeTextData` tokens inside it. CommonMark strips one space from each end when the content both
/// begins and ends with one and is not nothing but spaces; those two are the padding and what is
/// left is the data. A violation is whitespace the strip did not account for, so a single space that
/// was stripped is quiet and one that was not is not: `` ` a` `` is reported, `` ` a ` `` is not.
fn padded_runs(raw: &str, base: usize) -> Vec<((usize, usize), bool)> {
    let delimiter = raw.bytes().take_while(|&byte| byte == b'`').count();
    // The closing run is as long as the opening one; that is what made this a code span.
    let content = &raw[delimiter..raw.len() - delimiter];
    let content_start = base + delimiter;
    let content_end = content_start + content.len();

    let padded = content.len() >= 2
        && content.starts_with(' ')
        && content.ends_with(' ')
        && !content.bytes().all(|byte| byte == b' ');
    let data = if padded {
        &content[1..content.len() - 1]
    } else {
        content
    };
    let data_start = content_start + usize::from(padded);

    // micromark splits the data at line endings and emits no token for an empty piece, and only the
    // first and last of what is left are examined: `` `a \nb` `` is quiet, `` ` a\nb` `` is not.
    let Some((first, last)) = outer_chunks(data, data_start) else {
        return Vec::new();
    };

    let leading = Edge::leading(&data[first.0 - data_start..first.1 - data_start], padded);
    let trailing = Edge::trailing(&data[last.0 - data_start..last.1 - data_start], padded);
    // A space on both sides that the strip already took is safe to delete along with the extra one
    // beside it, so the report covers the padding too — unless either side abuts a backtick, where
    // deleting would change what the code span says.
    let remove_padding = leading.count > 0
        && trailing.count > 0
        && padded
        && !leading.abuts_backtick
        && !trailing.abuts_backtick;

    let mut runs = Vec::new();
    if leading.count > 0 {
        let from = if remove_padding {
            content_start
        } else {
            first.0
        };
        let length = leading.count + usize::from(remove_padding);
        runs.push(((from, from + length), true));
    }
    if trailing.count > 0 {
        let to = if remove_padding { content_end } else { last.1 };
        let length = trailing.count + usize::from(remove_padding);
        runs.push(((to - length, to), false));
    }
    runs
}

/// One end of a code span's data: how much whitespace sits there, and whether a backtick is beside
/// it. markdownlint counts one less in the second case when nothing was stripped, because `` `` `x ``
/// is how a literal backtick is written and the space before it belongs to the code.
struct Edge {
    count: usize,
    abuts_backtick: bool,
}

impl Edge {
    /// micromark's `/^(\s+)(\S)/` over the first data token.
    fn leading(text: &str, padded: bool) -> Self {
        let width = text.len() - text.trim_start_matches(char::is_whitespace).len();
        // Nothing but whitespace means the pattern does not match at all, so there is no count.
        let next = (width < text.len()).then(|| text.as_bytes()[width]);
        Edge::new(width, next, padded)
    }

    /// micromark's `/(\S)(\s+)$/` over the last data token.
    fn trailing(text: &str, padded: bool) -> Self {
        let width = text.len() - text.trim_end_matches(char::is_whitespace).len();
        // A multi-byte character before the run is asked about by its last byte, which is a
        // continuation byte and so is never a backtick — the same answer the character gives.
        let previous = (width < text.len()).then(|| text.as_bytes()[text.len() - width - 1]);
        Edge::new(width, previous, padded)
    }

    fn new(width: usize, neighbour: Option<u8>, padded: bool) -> Self {
        let abuts_backtick = neighbour == Some(b'`');
        Self {
            count: match neighbour {
                Some(_) if abuts_backtick && !padded => width.saturating_sub(1),
                Some(_) => width,
                None => 0,
            },
            abuts_backtick,
        }
    }
}

/// The absolute byte ranges of the first and last non-empty line of `data`, or `None` when it holds
/// no text at all — a code span of nothing but line endings has no data token to examine.
fn outer_chunks(data: &str, base: usize) -> Option<((usize, usize), (usize, usize))> {
    let mut first = None;
    let mut last = None;
    let mut offset = 0;
    for line in data.split(['\n', '\r']) {
        if !line.is_empty() {
            let chunk = (base + offset, base + offset + line.len());
            if first.is_none() {
                first = Some(chunk);
            }
            last = Some(chunk);
        }
        // `\r\n` splits into an empty piece the offset still has to step over.
        offset += line.len() + 1;
    }
    Some((first?, last?))
}

impl RuleLinter for MD038Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() != "inline" {
            return;
        }
        let runs = self.runs(*node);
        let found = {
            let source = self.context.get_document_content();
            runs.iter()
                .map(|run| self.violation(run, &source))
                .collect::<Vec<_>>()
        };
        self.violations.extend(found);
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD038: Rule = Rule {
    id: "MD038",
    alias: "no-space-in-code",
    tags: &["whitespace", "code"],
    description: "Spaces inside code span elements",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD038Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// `(line, column)` of one reported run, both 1-based, which is markdownlint's `errorRange`
    /// start. The column is where the whitespace begins, not where the code span does.
    type Position = (usize, usize);

    fn positions(source: &str) -> Vec<Position> {
        let config = test_config_with_rules(vec![("no-space-in-code", RuleSeverity::Error)]);
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
    /// MD038's defaults.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so the one case with a multi-byte
    /// character before the reported space is asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &[Position])] = &[
        // One space at either end and not both is the whole rule.
        ("`a`\n", &[]),
        ("` a`\n", &[(1, 2)]),
        ("`a `\n", &[(1, 3)]),
        ("` a `\n", &[]),
        ("`  a  `\n", &[(1, 2), (1, 5)]),
        ("`  a`\n", &[(1, 2)]),
        ("`a  `\n", &[(1, 3)]),
        ("`\ta`\n", &[(1, 2)]),
        ("`a\t`\n", &[(1, 3)]),
        ("` `\n", &[]),
        ("`  `\n", &[]),
        ("`` a``\n", &[(1, 3)]),
        ("``  `a`  ``\n", &[(1, 4), (1, 8)]),
        ("`` `a` ``\n", &[]),
        ("```  a  ```\n", &[(1, 4), (1, 7)]),
        // Only the first and last line of a multi-line span are examined.
        ("`a\nb`\n", &[]),
        ("` a\nb`\n", &[(1, 2)]),
        ("`a\nb `\n", &[(2, 2)]),
        ("`a\n b`\n", &[]),
        ("`a \nb`\n", &[]),
        ("` a\nb `\n", &[]),
        ("`` ``\n", &[]),
        ("```a```\n", &[]),
        // The same span in every inline context, and the ones that are not spans at all.
        ("x ` a` y `b ` z\n", &[(1, 4), (1, 12)]),
        ("# h ` a`\n", &[(1, 6)]),
        ("> ` a`\n", &[(1, 4)]),
        ("- ` a`\n", &[(1, 4)]),
        ("| h |\n|---|\n| ` a` |\n", &[(3, 4)]),
        ("[` a`](http://x)\n", &[(1, 3)]),
        ("**` a`**\n", &[(1, 4)]),
        ("` a``b `\n", &[]),
        ("<div>` a`</div>\n", &[]),
        ("$x` a`y$\n", &[]),
        ("` a`\n\n`b `\n", &[(1, 2), (3, 3)]),
        // Line endings, and content that is nothing but whitespace.
        ("`a \n`\n", &[(1, 3)]),
        ("`\n a`\n", &[(2, 1)]),
        ("`a\n`\n", &[]),
        ("`\n`\n", &[]),
        ("` \n `\n", &[]),
        ("`  a\n  b  `\n", &[(1, 2), (2, 4)]),
        ("`a\r\nb `\n", &[(2, 2)]),
        ("`a\rb `\n", &[(2, 2)]),
        ("` x\ny `\n", &[]),
        // Whitespace other than a space, and delimiters longer than one backtick.
        ("`\t\ta`\n", &[(1, 2)]),
        ("` \u{a0}a`\n", &[(1, 2)]),
        ("`a\u{a0} `\n", &[(1, 3)]),
        ("`` a` `\n", &[]),
        ("``a ``\n", &[(1, 4)]),
        ("` \t `\n", &[]),
        ("`\t `\n", &[]),
        ("` \t`\n", &[]),
        ("`   `\n", &[]),
        ("` \u{b} a`\n", &[(1, 2)]),
        ("``  a  ``\n", &[(1, 3), (1, 6)]),
        ("`  ``a``  `\n", &[(1, 3), (1, 9)]),
        ("` `` `\n", &[]),
        ("``` `\n", &[]),
        ("` ```\n", &[]),
        ("`\u{2003}a`\n", &[(1, 2)]),
        ("`a\u{2003}`\n", &[(1, 3)]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, positions(source).as_slice(), "source {source:?}");
        }
    }

    /// A line separator before the reported space. markdownlint counts UTF-16 units, quickmark
    /// counts bytes, and U+2028 is three of one and one of the other. That is the byte-column
    /// convention every rule shares, not an MD038 difference.
    #[test]
    fn positions_count_bytes() {
        // markdownlint: [(1, 5)]
        assert_eq!(vec![(1, 7)], positions("`a\u{2028}b `\n"));
    }

    /// A code span the parser did not close is text, so its backticks and spaces are none of this
    /// rule's business.
    #[test]
    fn an_unclosed_run_is_not_a_code_span() {
        assert!(positions("This has `` empty code spans.\n").is_empty());
        assert!(positions("a ` b\n").is_empty());
        assert!(positions("a `` b ` c\n").is_empty());
    }

    /// markdownlint quotes the whole code span, backticks and all, keeping whichever end the report
    /// is about when it has to shorten it.
    #[test]
    fn a_report_quotes_the_code_span() {
        let config = test_config_with_rules(vec![("no-space-in-code", RuleSeverity::Error)]);
        for (source, message) in [
            (
                "see `a ` here\n",
                "Spaces inside code span elements [Context: \"`a `\"]",
            ),
            (
                "see ` a` here\n",
                "Spaces inside code span elements [Context: \"` a`\"]",
            ),
        ] {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config.clone(), source);
            let violations = linter.analyze();
            assert_eq!(1, violations.len(), "{source:?}");
            assert_eq!(message, violations[0].message(), "{source:?}");
        }
    }
}
