use linkify::{LinkFinder, LinkKind};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{Rule, RuleType},
};

// MD044-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD044ProperNamesTable {
    #[serde(default)]
    pub names: Vec<String>,
    /// markdownlint scans code and HTML unless it is explicitly told not to, which `bool::default`
    /// has the other way round.
    #[serde(default = "on")]
    pub code_blocks: bool,
    #[serde(default = "on")]
    pub html_elements: bool,
}

fn on() -> bool {
    true
}

impl Default for MD044ProperNamesTable {
    fn default() -> Self {
        Self {
            names: Vec::new(),
            code_blocks: true,
            html_elements: true,
        }
    }
}

/// One run of scannable text. Always a single line: markdownlint reports at the token's own start
/// line and counts the match's offset along it, and every token it scans is one line long.
#[derive(Clone, Copy)]
struct Scan {
    from: usize,
    to: usize,
    row: usize,
    column: usize,
}

pub(crate) struct MD044Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    /// The configured names as compiled patterns, longest name first and then in byte order — the
    /// order markdownlint checks them in, so a longer name claims its text before a shorter one can.
    patterns: Vec<(String, Regex)>,
    all_names: HashSet<String>,
    finder: LinkFinder,
}

impl MD044Linter {
    pub fn new(context: Rc<Context>) -> Self {
        let config = &context.config.linters.settings.proper_names;
        let mut names = config.names.clone();
        names.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        let patterns = names
            .iter()
            .filter_map(|name| Some((name.clone(), name_pattern(name)?)))
            .collect();
        let all_names: HashSet<String> = config.names.iter().cloned().collect();

        let mut finder = LinkFinder::new();
        finder.url_must_have_scheme(false);

        Self {
            context,
            violations: Vec::new(),
            patterns,
            all_names,
            finder,
        }
    }

    fn config(&self) -> &MD044ProperNamesTable {
        &self.context.config.linters.settings.proper_names
    }

    fn run(&mut self, root: Node) {
        if self.patterns.is_empty() {
            return;
        }

        let mut reports = Vec::new();
        {
            let source = self.context.document_content.borrow();
            let mut scans = Vec::new();
            self.collect(root, &source, &mut scans);

            // Ranges no name may be reported in, accumulated in markdownlint's order: every match
            // of every name, plus the bare URLs of a run once something has matched in it.
            let mut exclusions: Vec<(usize, usize)> = Vec::new();
            // Whether a run's bare URLs have been looked for yet. markdownlint looks once, on the
            // first name that matches in a run, and relies on `exclusions` after that.
            let mut scanned = vec![false; scans.len()];

            for (name, pattern) in &self.patterns {
                for (index, scan) in scans.iter().enumerate() {
                    let text = &source[scan.from..scan.to];
                    for captures in pattern.captures_iter(text) {
                        let Some(matched) = captures.get(2) else {
                            continue;
                        };
                        // markdownlint's ranges include the last character, and `hasOverlap`
                        // compares them that way.
                        let range = (scan.from + matched.start(), scan.from + matched.end() - 1);
                        let hidden = if self.all_names.contains(matched.as_str())
                            || overlaps(&exclusions, range)
                        {
                            true
                        } else if scanned[index] {
                            false
                        } else {
                            let urls = self.autolink_ranges(text, scan.from);
                            exclusions.extend(urls.iter().copied());
                            scanned[index] = true;
                            overlaps(&urls, range)
                        };
                        if !hidden {
                            let column = scan.column + matched.start();
                            reports.push(self.violation(name, matched.as_str(), scan.row, column));
                        }
                        exclusions.push(range);
                    }
                }
            }
        }
        self.violations.append(&mut reports);
    }

    /// The bare URLs inside one scanned run, as the byte ranges markdownlint excludes. It reparses
    /// the run and drops whatever comes back as a `literalAutolink`; `linkify` finds the same runs
    /// here, and [`is_gfm_autolink`] narrows them to the schemes GFM recognises.
    fn autolink_ranges(&self, text: &str, base: usize) -> Vec<(usize, usize)> {
        self.finder
            .links(text)
            .filter(|link| link.kind() == &LinkKind::Url && is_gfm_autolink(link.as_str()))
            .map(|link| (base + link.start(), base + link.end() - 1))
            .collect()
    }

    fn violation(&self, expected: &str, actual: &str, row: usize, column: usize) -> RuleViolation {
        RuleViolation::new(
            &MD044,
            format!("Expected: {expected}; Actual: {actual}"),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: 0,
                end_byte: 0,
                start_point: crate::ast::Point { row, column },
                end_point: crate::ast::Point {
                    row,
                    column: column + actual.len(),
                },
            }),
        )
    }

    /// Every run of text markdownlint scans, in document order.
    ///
    /// It works from micromark's token types: `data` always, `codeFlowValue` and `codeTextData` when
    /// `code_blocks` is on, `htmlFlowData` and `htmlTextData` when `html_elements` is on. It descends
    /// into every token except a code fence's own markers, a link reference definition, and a
    /// reference or a resource — so a link's *label* is scanned and its destination and title are
    /// not, and those kinds have no counterpart to skip here.
    fn collect(&self, node: Node, source: &str, out: &mut Vec<Scan>) {
        match node.kind() {
            "text" => self.push_lines(node.start_byte(), node.end_byte(), 0, source, out),
            // micromark's `mathTextData` is not a scanned type, so math hides whatever is in it.
            "math" => {}
            "code_span" => {
                if self.config().code_blocks {
                    // `codeTextData` sits between the backtick runs, not inside them.
                    let (from, to) = (node.start_byte(), node.end_byte());
                    let run = source[from..]
                        .bytes()
                        .take_while(|&byte| byte == b'`')
                        .count();
                    self.push_lines(from + run, to.saturating_sub(run), 0, source, out);
                }
            }
            "code_fence_content" => {
                if self.config().code_blocks {
                    let indent = node
                        .parent()
                        .map_or(0, |block| leading_whitespace(source, block.start_byte(), 3));
                    self.push_lines(node.start_byte(), node.end_byte(), indent, source, out);
                }
            }
            "indented_code_block" => {
                if self.config().code_blocks {
                    // micromark's `linePrefix` takes the indentation — four columns, counting a tab
                    // as one — and the `codeFlowValue` starts after it.
                    self.push_lines(node.start_byte(), node.end_byte(), 4, source, out);
                }
            }
            "html_inline" => {
                if self.config().html_elements {
                    self.push_lines(node.start_byte(), node.end_byte(), 0, source, out);
                }
            }
            "html_block" => self.push_html(node.start_byte(), node.end_byte(), source, out),
            // `<https://x.com>` is an `autolink` whose protocol is not a scanned type, so a name in
            // it is invisible. A link's own label *is* scanned, so only the bracketed form stops
            // here — a bare URL stays inside a `text` run and is handled by [`Self::autolink_ranges`].
            "link" | "image" => {
                if source.as_bytes().get(node.start_byte()) != Some(&b'<') {
                    self.collect_children(node, source, out);
                }
            }
            _ => self.collect_children(node, source, out),
        }
    }

    fn collect_children(&self, node: Node, source: &str, out: &mut Vec<Scan>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect(child, source, out);
        }
    }

    /// The scannable text of an HTML block.
    ///
    /// markdownlint reparses a block that is not a comment and scans the tokens that come out, so
    /// `<div>\ngithub\n</div>` reports even with `html_elements` off, while a comment stays one
    /// `htmlFlowData` per line and does not. Splitting on the `<...>` runs reproduces that without
    /// reparsing: outside a run is content, inside one is an element. It does not reproduce the
    /// reparse's *inline* structure, so a code span in an HTML block is scanned as content.
    fn push_html(&self, from: usize, to: usize, source: &str, out: &mut Vec<Scan>) {
        let to = to.min(source.len());
        if is_comment(&source[from..to]) {
            if self.config().html_elements {
                self.push_lines(from, to, 0, source, out);
            }
            return;
        }
        let mut at = from;
        while at < to {
            let Some(open) = tag_start(source, at, to) else {
                self.push_lines(at, to, 0, source, out);
                return;
            };
            self.push_lines(at, open, 0, source, out);
            let close = source[open..to]
                .find('>')
                .map_or(to, |offset| open + offset + 1);
            if self.config().html_elements {
                self.push_lines(open, close, 0, source, out);
            }
            at = close;
        }
    }

    /// Splits `[from, to)` into one [`Scan`] per line, each starting after up to `skip` columns of
    /// leading whitespace.
    fn push_lines(&self, from: usize, to: usize, skip: usize, source: &str, out: &mut Vec<Scan>) {
        let to = to.min(source.len());
        let mut start = from;
        while start < to {
            let end = source[start..to]
                .find(['\n', '\r'])
                .map_or(to, |offset| start + offset);
            let from = skip_whitespace(source, start, end, skip);
            if from < end {
                let point = self.context.point_at(from);
                out.push(Scan {
                    from,
                    to: end,
                    row: point.row,
                    column: point.column,
                });
            }
            start = end + 1;
        }
    }
}

/// The byte after up to `limit` columns of leading whitespace, which is where micromark's
/// `linePrefix` ends. A tab counts as one column.
fn skip_whitespace(source: &str, from: usize, to: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    let mut at = from;
    while at < to && at - from < limit && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    at
}

/// The number of leading space and tab bytes at `from`, up to `limit`.
fn leading_whitespace(source: &str, from: usize, limit: usize) -> usize {
    skip_whitespace(source, from, source.len(), limit) - from
}

/// The next `<` that opens an HTML tag, which is one followed by a name, a slash or a `!`.
fn tag_start(source: &str, from: usize, to: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    (from..to).find(|&at| {
        bytes[at] == b'<' && matches!(bytes.get(at + 1), Some(b) if b.is_ascii_alphabetic() || matches!(b, b'/' | b'!' | b'?'))
    })
}

/// Whether an HTML block is a comment, which is the one markdownlint does not reparse.
fn is_comment(text: &str) -> bool {
    let text = text.trim_end_matches(['\n', '\r', ' ', '\t']);
    if !text.starts_with("<!--") || !text.ends_with("-->") {
        return false;
    }
    // `get` because a `-->` can start before the `<!--` ends, where JavaScript's `slice` gives "".
    let comment = text.get(4..text.len() - 3).unwrap_or("");
    !comment.starts_with('>') && !comment.starts_with("->") && !comment.ends_with('-')
}

/// Whether two inclusive ranges overlap — markdownlint's `hasOverlap` on ranges that never cross a
/// line boundary.
fn overlaps(ranges: &[(usize, usize)], range: (usize, usize)) -> bool {
    ranges
        .iter()
        .any(|&(from, to)| from <= range.1 && range.0 <= to)
}

/// markdownlint's pattern for one name: `(\b_*)(name)_*\b`, case-insensitive, with a boundary
/// dropped when that end of the name is not a word character. Both `\b`s are ASCII-only, as they are
/// in a JavaScript pattern without the `u` flag.
fn name_pattern(name: &str) -> Option<Regex> {
    if name.is_empty() {
        return None;
    }
    let start = if is_word_char(name.chars().next()?) {
        r"((?-u:\b_*))"
    } else {
        "()"
    };
    let end = if is_word_char(name.chars().last()?) {
        r"_*(?-u:\b)"
    } else {
        ""
    };
    Regex::new(&format!("(?i){start}({}){end}", regex::escape(name))).ok()
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether a URL starts with something GFM's autolink-literal extension recognises, which is what
/// makes micromark turn it into a `literalAutolink` and markdownlint leave the names inside it alone.
fn is_gfm_autolink(url: &str) -> bool {
    ["http://", "https://", "www."].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

impl RuleLinter for MD044Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "document" {
            self.run(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD044: Rule = Rule {
    id: "MD044",
    alias: "proper-names",
    tags: &["spelling"],
    description: "Proper names should have the correct capitalization",
    rule_type: RuleType::Document,
    required_nodes: &["document"],
    new_linter: |context| Box::new(MD044Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD044ProperNamesTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    const NAMES: [&str; 5] = ["GitHub", "Node.js", "JavaScript", "CSS", "@scope/pkg"];

    /// A report: the 1-based line and column of the name, then the two halves of the message.
    type Report = (usize, usize, &'static str, &'static str);
    type Found = (usize, usize, String, String);
    /// A case's name, its document, and the reports markdownlint makes on it with `code_blocks` and
    /// `html_elements` on and then with both off.
    type Case = (
        &'static str,
        &'static str,
        &'static [Report],
        &'static [Report],
    );

    fn test_config(code_blocks: bool, html_elements: bool) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("proper-names", RuleSeverity::Error)],
            LintersSettingsTable {
                proper_names: MD044ProperNamesTable {
                    names: NAMES.iter().map(|&name| name.to_string()).collect(),
                    code_blocks,
                    html_elements,
                },
                ..Default::default()
            },
        )
    }

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, expected, actual)| {
                (line, column, expected.to_string(), actual.to_string())
            })
            .collect()
    }

    /// markdownlint-cli2 sorts a file's reports by line and then by message, so neither order here
    /// is the order the reports are made in.
    fn reports(input: &str, code_blocks: bool, html_elements: bool) -> Vec<Found> {
        let config = test_config(code_blocks, html_elements);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let mut found: Vec<Found> = linter
            .analyze()
            .iter()
            .map(|violation| {
                let message = violation.message();
                let (expected, actual) = message.split_once("; Actual: ").unwrap_or((message, ""));
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    expected.trim_start_matches("Expected: ").to_string(),
                    actual.to_string(),
                )
            })
            .collect();
        found.sort();
        found
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "lowercase in a paragraph",
                "github is a site\n",
                &[(1, 1, "GitHub", "github")],
                &[(1, 1, "GitHub", "github")],
            ),
            ("the correct capitalization", "GitHub is a site\n", &[], &[]),
            (
                "in an atx heading",
                "# github heading\n",
                &[(1, 3, "GitHub", "github")],
                &[(1, 3, "GitHub", "github")],
            ),
            (
                "in a list item",
                "- github item\n",
                &[(1, 3, "GitHub", "github")],
                &[(1, 3, "GitHub", "github")],
            ),
            (
                "in a block quote",
                "> github quoted\n",
                &[(1, 3, "GitHub", "github")],
                &[(1, 3, "GitHub", "github")],
            ),
            (
                "in a table cell",
                "| a |\n| - |\n| github |\n",
                &[(3, 3, "GitHub", "github")],
                &[(3, 3, "GitHub", "github")],
            ),
            (
                "in a code span",
                "see `github` here\n",
                &[(1, 6, "GitHub", "github")],
                &[],
            ),
            (
                "in a fenced code block",
                "```\ngithub\n```\n",
                &[(2, 1, "GitHub", "github")],
                &[],
            ),
            (
                "in an indented code block",
                "text\n\n    github\n",
                &[(3, 5, "GitHub", "github")],
                &[],
            ),
            (
                "between inline html elements",
                "a <b>github</b> b\n",
                &[(1, 6, "GitHub", "github")],
                &[(1, 6, "GitHub", "github")],
            ),
            (
                "in an html block",
                "<div>\ngithub\n</div>\n",
                &[(2, 1, "GitHub", "github")],
                &[(2, 1, "GitHub", "github")],
            ),
            (
                "in a link label",
                "[github](https://x.com)\n",
                &[(1, 2, "GitHub", "github")],
                &[(1, 2, "GitHub", "github")],
            ),
            (
                "in a link destination",
                "[label](https://github.com/x)\n",
                &[],
                &[],
            ),
            (
                "in a link title",
                "[label](https://x.com \"github title\")\n",
                &[],
                &[],
            ),
            (
                "in a full reference link's label",
                "[github][ref]\n\n[ref]: https://x.com\n",
                &[(1, 2, "GitHub", "github")],
                &[(1, 2, "GitHub", "github")],
            ),
            (
                "in a reference link's reference",
                "[label][github]\n\n[github]: https://x.com\n",
                &[],
                &[],
            ),
            (
                "in a link reference definition",
                "[github]: https://x.com\n\ntext\n",
                &[],
                &[],
            ),
            (
                "in an image's alt text",
                "![github alt](x.png)\n",
                &[(1, 3, "GitHub", "github")],
                &[(1, 3, "GitHub", "github")],
            ),
            ("in a bare url", "see https://github.com/x here\n", &[], &[]),
            (
                "in an angle bracket autolink",
                "see <https://github.com/x> here\n",
                &[],
                &[],
            ),
            (
                "in an html comment",
                "<!-- github -->\n",
                &[(1, 6, "GitHub", "github")],
                &[],
            ),
            (
                "in emphasis",
                "*github* and _github_\n",
                &[(1, 2, "GitHub", "github"), (1, 15, "GitHub", "github")],
                &[(1, 2, "GitHub", "github"), (1, 15, "GitHub", "github")],
            ),
            (
                "on two lines of one paragraph",
                "first line has github\nsecond line has github too\n",
                &[(1, 16, "GitHub", "github"), (2, 17, "GitHub", "github")],
                &[(1, 16, "GitHub", "github"), (2, 17, "GitHub", "github")],
            ),
            (
                "in three capitalizations",
                "node.JS and NODE.js and node.js\n",
                &[
                    (1, 1, "Node.js", "node.JS"),
                    (1, 13, "Node.js", "NODE.js"),
                    (1, 25, "Node.js", "node.js"),
                ],
                &[
                    (1, 1, "Node.js", "node.JS"),
                    (1, 13, "Node.js", "NODE.js"),
                    (1, 25, "Node.js", "node.js"),
                ],
            ),
            (
                "starting with punctuation",
                "@SCOPE/PKG and @scope/pkg\n",
                &[(1, 1, "@scope/pkg", "@SCOPE/PKG")],
                &[(1, 1, "@scope/pkg", "@SCOPE/PKG")],
            ),
            (
                "in emphasis and in strong emphasis",
                "_github_ and __github__\n",
                &[(1, 2, "GitHub", "github"), (1, 16, "GitHub", "github")],
                &[(1, 2, "GitHub", "github"), (1, 16, "GitHub", "github")],
            ),
            (
                "in a setext heading",
                "Setext github\n=============\n",
                &[(1, 8, "GitHub", "github")],
                &[(1, 8, "GitHub", "github")],
            ),
            (
                "in front matter",
                "---\ntitle: github\n---\n\n# H\n",
                &[],
                &[],
            ),
            ("in math", "$github$ math\n", &[], &[]),
            (
                "three times on one line",
                "github github github\n",
                &[
                    (1, 1, "GitHub", "github"),
                    (1, 8, "GitHub", "github"),
                    (1, 15, "GitHub", "github"),
                ],
                &[
                    (1, 1, "GitHub", "github"),
                    (1, 8, "GitHub", "github"),
                    (1, 15, "GitHub", "github"),
                ],
            ),
            (
                "alongside an exact match",
                "css and Css and CSS\n",
                &[(1, 1, "CSS", "css"), (1, 9, "CSS", "Css")],
                &[(1, 1, "CSS", "css"), (1, 9, "CSS", "Css")],
            ),
            (
                "alone on a line",
                "javascript\n",
                &[(1, 1, "JavaScript", "javascript")],
                &[(1, 1, "JavaScript", "javascript")],
            ),
            ("inside longer words", "xgithub and githubx\n", &[], &[]),
            (
                "in a code span and in text",
                "`github` and github\n",
                &[(1, 2, "GitHub", "github"), (1, 14, "GitHub", "github")],
                &[(1, 14, "GitHub", "github")],
            ),
            (
                "in a list item's continuation",
                "- item\n\n  github in a continuation\n",
                &[(3, 3, "GitHub", "github")],
                &[(3, 3, "GitHub", "github")],
            ),
            (
                "in a level six heading",
                "###### github\n",
                &[(1, 8, "GitHub", "github")],
                &[(1, 8, "GitHub", "github")],
            ),
            (
                "in an attribute inside an html block",
                "<div>\n<span title=\"github\">x</span>\n</div>\n",
                &[(2, 14, "GitHub", "github")],
                &[],
            ),
            (
                "on one line of a multi line comment",
                "<!-- multi\ngithub here\n-->\n",
                &[(2, 1, "GitHub", "github")],
                &[],
            ),
            (
                "in tab indented code",
                "\tgithub\n",
                &[(1, 2, "GitHub", "github")],
                &[],
            ),
            (
                "after a stray angle bracket",
                "a < b and github\n",
                &[(1, 11, "GitHub", "github")],
                &[(1, 11, "GitHub", "github")],
            ),
            (
                "in a one line html block",
                "<div>github</div>\n",
                &[(1, 6, "GitHub", "github")],
                &[(1, 6, "GitHub", "github")],
            ),
            (
                "in an indented fenced code block",
                "  ```\n  github\n  ```\n",
                &[(2, 3, "GitHub", "github")],
                &[],
            ),
            (
                "in a tilde fence and in indented code",
                "  ~~~js\n  github\n  ~~~\n\n    github\n",
                &[(2, 3, "GitHub", "github"), (5, 5, "GitHub", "github")],
                &[],
            ),
            (
                "after an angle bracket quoted in an attribute",
                "<div>\n<a title=\"x>y\">github</a>\n</div>\n",
                &[(2, 16, "GitHub", "github")],
                &[(2, 16, "GitHub", "github")],
            ),
        ];
        for (name, input, with, without) in cases {
            assert_eq!(
                owned(with),
                reports(input, true, true),
                "{name}, both settings on"
            );
            assert_eq!(
                owned(without),
                reports(input, false, false),
                "{name}, both settings off"
            );
        }
    }

    /// markdownlint reparses an HTML block that is not a comment and scans the tokens that come
    /// out, so the code span in it is a `codeTextData` and `code_blocks` still governs it. Nothing
    /// here reparses a block, so the text outside the `<...>` runs is scanned as content.
    ///
    /// markdownlint: `[]`
    #[test]
    fn a_code_span_in_an_html_block_is_a_known_difference() {
        assert_eq!(
            owned(&[(2, 2, "GitHub", "github")]),
            reports("<div>\n`github`\n</div>\n", false, false)
        );
    }

    #[test]
    fn a_report_covers_the_name() {
        let config = test_config(true, true);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, "a github b\n");
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 2, 0, 8),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }
}
