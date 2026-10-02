use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{md037::is_escaped, Context, Rule, RuleLinter, RuleType},
};

// MD052-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD052ReferenceLinksImagesTable {
    #[serde(default)]
    pub shortcut_syntax: bool,
    // Not `#[serde(default)]`: a settings table that sets only `shortcut_syntax` still gets the
    // default ignored labels, which is what markdownlint's `config.ignored_labels || ["x"]` does.
    #[serde(default = "default_ignored_labels")]
    pub ignored_labels: Vec<String>,
}

fn default_ignored_labels() -> Vec<String> {
    vec!["x".to_string()]
}

impl Default for MD052ReferenceLinksImagesTable {
    fn default() -> Self {
        Self {
            shortcut_syntax: false,
            ignored_labels: default_ignored_labels(),
        }
    }
}

/// A run of two or more adjacent bracket groups — `[text][label]`, `[label][]`, `[a][b][c]` — or a
/// lone one. Leftmost-first alternation, so the chain always wins over the shortcut inside it.
static REFERENCE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:\[[^\[\]]*\]){2,}|\[[^\[\]]+\]").expect("Invalid reference pattern")
});

/// Leaves whose contents are literal, so brackets in one open no label.
const LITERAL: &[&str] = &["code_span", "math", "html_inline"];

#[derive(Debug, Clone)]
struct ReferenceLink {
    label: String,
    range: crate::ast::NodeRange,
    is_shortcut: bool,
}

pub(crate) struct MD052Linter {
    context: Rc<Context>,
    references: Vec<ReferenceLink>,
}

impl MD052Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            references: Vec::new(),
        }
    }

    fn normalize_reference(&self, label: &str) -> String {
        // Normalize reference labels according to CommonMark spec:
        // - Convert to lowercase
        // - Trim whitespace
        // - Collapse consecutive whitespace to single spaces
        label
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Finds the reference syntax in one inline subtree.
    ///
    /// A reference the parser resolved is a `link` or `image` node and its brackets are gone from the
    /// text, so what is left to find here is exactly the syntax that resolved to nothing — which is
    /// what MD052 reports, and why the rule needs no set of definitions of its own. Two kinds of span
    /// are still off limits: the literal leaves, and a resolved link's destination, which is not one
    /// of its children.
    fn collect_references(&mut self, root: Node) {
        let mut found = Vec::new();
        {
            let source = self.context.document_content.borrow();
            let (from, to) = (root.start_byte(), root.end_byte());
            let excluded = literal_spans(root);
            for capture in REFERENCE_PATTERN.find_iter(&source[from..to]) {
                let start = from + capture.start();
                let end = from + capture.end();
                // Only the brackets have to sit outside a literal span: a code span *inside* a
                // label leaves the reference real, and markdownlint reports
                // ``[the `Sized` trait][sized]``, while `` `[x][y]` `` is code and is not reported.
                if covers(&excluded, start) || covers(&excluded, end - 1) {
                    continue;
                }
                // An escaped bracket opens no label, so `\[a][b]` leaves only a shortcut.
                if is_escaped(&source, start) {
                    continue;
                }
                // An image's `!` is part of what markdownlint reports.
                let preceded_by_bang = start
                    .checked_sub(1)
                    .and_then(|before| source.as_bytes().get(before))
                    == Some(&b'!');
                let start = if preceded_by_bang { start - 1 } else { start };
                let Some(reference) = self.classify(capture.as_str(), start, end) else {
                    continue;
                };
                found.push(reference);
            }
        }
        self.references.extend(found);
    }

    /// Works out which label a bracket run refers to, or `None` when micromark would not have made a
    /// reference out of it at all.
    fn classify(&self, chain: &str, start: usize, end: usize) -> Option<ReferenceLink> {
        let groups = bracket_groups(chain);
        let (label, is_shortcut) = match groups.len() {
            0 => return None,
            1 => (groups[0], true),
            // `[label][]` is collapsed and refers to its own label; anything longer refers to the
            // last group, which is why `[a][b][c]` reports `c`.
            _ if groups[groups.len() - 1].is_empty() => (groups[0], false),
            // micromark only pairs two labels when the first one has content, so `[][b]` is a
            // shortcut rather than a full reference.
            _ if groups[0].trim().is_empty() => return None,
            _ => (groups[groups.len() - 1], false),
        };
        if label.trim().is_empty() {
            return None;
        }
        Some(ReferenceLink {
            label: self.normalize_reference(label),
            range: self.range_on_line(start, end),
            is_shortcut,
        })
    }

    /// A reference spanning lines is reported on the line it opens on, and only as far as that line
    /// goes — markdownlint takes its context from a single line too.
    fn range_on_line(&self, start: usize, end: usize) -> crate::ast::NodeRange {
        let start_point = self.context.point_at(start);
        let lines = self.context.lines.borrow();
        let line_end = lines.get(start_point.row).map_or(end, |line| {
            self.context.line_start_byte(start_point.row) + line.len()
        });
        let end = end.min(line_end);
        crate::ast::NodeRange {
            start_byte: start,
            end_byte: end,
            start_point,
            end_point: self.context.point_at(end),
        }
    }
}

fn covers(spans: &[(usize, usize)], byte: usize) -> bool {
    spans.iter().any(|&(from, to)| from <= byte && byte < to)
}

/// The spans inside `root` that hold no reference syntax: the literal leaves, and each resolved
/// link's or image's destination, which is the part of the node its children do not cover.
fn literal_spans(root: Node) -> Vec<(usize, usize)> {
    let mut excluded = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if LITERAL.contains(&kind) {
            excluded.push((node.start_byte(), node.end_byte()));
        } else if kind == "link" || kind == "image" {
            let last = node
                .child_count()
                .checked_sub(1)
                .and_then(|index| node.child(index));
            match last {
                Some(last) => excluded.push((last.end_byte(), node.end_byte())),
                None => excluded.push((node.start_byte(), node.end_byte())),
            }
        }
        for index in 0..node.child_count() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    excluded
}

/// The contents of each `[...]` group in a chain, in order.
fn bracket_groups(chain: &str) -> Vec<&str> {
    let bytes = chain.as_bytes();
    let mut groups = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'[' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b']' {
            end += 1;
        }
        groups.push(&chain[start..end.min(bytes.len())]);
        index = end + 1;
    }
    groups
}

impl RuleLinter for MD052Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.collect_references(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        let mut violations = Vec::new();
        let config = &self.context.config.linters.settings.reference_links_images;
        let ignored_labels: HashSet<String> = config
            .ignored_labels
            .iter()
            .map(|label| self.normalize_reference(label))
            .collect();

        for reference in std::mem::take(&mut self.references) {
            // Skip shortcut syntax unless explicitly enabled
            if reference.is_shortcut && !config.shortcut_syntax {
                continue;
            }
            if ignored_labels.contains(&reference.label) {
                continue;
            }
            violations.push(RuleViolation::new(
                &MD052,
                format!(
                    "Missing link or image reference definition: \"{}\"",
                    reference.label
                ),
                self.context.file_path.clone(),
                range_from_node_range(&reference.range),
            ));
        }

        violations
    }
}

pub const MD052: Rule = Rule {
    id: "MD052",
    alias: "reference-links-images",
    tags: &["links", "images"],
    description: "Reference links and images should use a label that is defined",
    rule_type: RuleType::Document,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD052Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD052ReferenceLinksImagesTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    /// `(line, column)` of one undefined reference, both 1-based, which is markdownlint's
    /// `errorRange` start.
    type Position = (usize, usize);

    fn config_with(table: MD052ReferenceLinksImagesTable) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("reference-links-images", RuleSeverity::Error)],
            LintersSettingsTable {
                reference_links_images: table,
                ..Default::default()
            },
        )
    }

    fn positions_with(config: crate::config::QuickmarkConfig, source: &str) -> Vec<Position> {
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

    fn positions(source: &str) -> Vec<Position> {
        positions_with(
            config_with(MD052ReferenceLinksImagesTable::default()),
            source,
        )
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD052's defaults: its line and its `errorRange` column.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so the one case with a multi-byte
    /// character before a reference is asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &[Position])] = &[
        ("[text][label]\n", &[(1, 1)]),
        ("[text][label]\n\n[label]: http://x\n", &[]),
        ("[label][]\n", &[(1, 1)]),
        ("[label][]\n\n[label]: http://x\n", &[]),
        ("[label]\n", &[]),
        ("[label]\n\n[label]: http://x\n", &[]),
        ("see ([dp][dp]) here\n", &[(1, 6)]),
        ("[[RFC][PATCH] title](http://x)\n", &[(1, 2)]),
        ("text[1][2]\n", &[(1, 5)]),
        ("a thing (see [the docs][docs]) here\n", &[(1, 14)]),
        ("f(x) and [a][b]\n", &[(1, 10)]),
        ("# [a][b] title\n", &[(1, 3)]),
        ("| a |\n|---|\n| [x][y] |\n", &[(3, 3)]),
        ("> [x][y]\n", &[(1, 3)]),
        ("- [x][y]\n", &[(1, 3)]),
        ("`[x][y]`\n", &[]),
        ("```\n[x][y]\n```\n", &[]),
        ("$[x][y]$\n", &[]),
        ("![alt][label]\n", &[(1, 1)]),
        ("![alt][label]\n\n[label]: http://x\n", &[]),
        ("[a]b][c]\n", &[]),
        ("[][]\n", &[]),
        ("[ ][ ]\n", &[]),
        ("[text][LABEL]\n\n[label]: http://x\n", &[]),
        ("[text][a  b]\n\n[a b]: http://x\n", &[]),
        ("[a][b] and [c][d]\n", &[(1, 1), (1, 12)]),
        ("[a][b]\n\n[c][d]\n", &[(1, 1), (3, 1)]),
        ("[text][x]\n", &[]),
        ("[a][b]\n\n[b]: http://x\n", &[]),
        ("[outer [inner][nope] text](http://x)\n", &[(1, 8)]),
        ("[outer [inner] text](http://x)\n", &[]),
        ("[label]: http://x\n", &[]),
        ("\\[a][b]\n", &[]),
        ("[a][b]", &[(1, 1)]),
        ("[a][b]\r\n[c][d]\r\n", &[(1, 1), (2, 1)]),
        ("[a\nb][c]\n", &[(1, 1)]),
        ("[x](http://y/[a][b])\n", &[]),
        ("<div>\n[a][b]\n</div>\n", &[]),
        ("[][b]\n", &[]),
        ("[ ][b]\n", &[]),
        ("[a][]\n", &[(1, 1)]),
        ("[a][ ]\n", &[]),
        ("[a][b][c]\n", &[(1, 1)]),
        ("[a[b][c]\n", &[(1, 3)]),
        ("[a][b] [c]\n", &[(1, 1)]),
        ("![][b]\n", &[]),
        ("[a]\n[b]\n", &[]),
        ("[a][b]\n[c][d]\n", &[(1, 1), (2, 1)]),
        ("text [x] more [y][z] end\n", &[(1, 15)]),
        (
            "[the `Sized` trait][sized] and [a][b]\n",
            &[(1, 1), (1, 32)],
        ),
        // Which lines define a label. Each is `[x][a]` after something that does or does not turn out
        // to be a definition of `a`, so the expectation is the whole question.
        ("[a]: /u\n\n[x][a]\n", &[]),
        ("[a]: /u\nbar\n\n[x][a]\n", &[]),
        ("[a]: /u\n  \"title\"\n\n[x][a]\n", &[]),
        ("[a]:\n/u\n\n[x][a]\n", &[]),
        ("   [a]: /u\n\n[x][a]\n", &[]),
        ("> [a]: /u\n\n[x][a]\n", &[]),
        ("- [a]: /u\n\n[x][a]\n", &[]),
        ("[x][a]\n\n[a]: /u\n", &[]),
        ("[x][a]\n\n[a]: /u \"t\" junk\n", &[(1, 1)]),
        // A bare destination may not contain a space, so this is an ordinary paragraph and
        // `previouspost` is undefined.
        ("[p]: {% post_url x %} [n]: {% y\n%}\n\n[a][p]\n", &[(4, 1)]),
        ("# [a]: /u\n\n[x][a]\n", &[(3, 1)]),
        ("| [a]: /u |\n|---|\n\n[x][a]\n", &[(4, 1)]),
        // Normalization: the label is reported lowercased, trimmed and whitespace-collapsed.
        ("[x][ a ]\n", &[(1, 1)]),
        ("[x][A  B]\n", &[(1, 1)]),
        ("[x][a]\n\n[A]: /u\n", &[]),
        ("[x][a b]\n\n[a  B]: /u\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, positions(source).as_slice(), "source {source:?}");
        }
    }

    /// A reference after a multi-byte character. markdownlint counts UTF-16 units, so its column is
    /// smaller than the byte-based one quickmark reports. That is the byte-column convention every
    /// rule shares, not an MD052 difference.
    #[test]
    fn positions_count_bytes() {
        // markdownlint: [(1, 3)]
        assert_eq!(vec![(1, 5)], positions("你 [a][b] 好\n"));
    }

    /// The label in the message is the normalized one, not the written one: markdownlint says `"abc"`
    /// for `[x][ABC]` and `"a b"` for `[y][A  B]`.
    #[test]
    fn labels_in_messages_are_normalized() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            config_with(MD052ReferenceLinksImagesTable::default()),
            "[x][ABC] and [y][A  B]\n",
        );
        let messages: Vec<String> = linter
            .analyze()
            .iter()
            .map(|violation| violation.message().to_string())
            .collect();
        assert_eq!(
            vec![
                r#"Missing link or image reference definition: "abc""#,
                r#"Missing link or image reference definition: "a b""#,
            ],
            messages
        );
    }

    #[test]
    fn shortcut_syntax_reports_lone_labels() {
        let source = "# H\n\n[a] and [b][c] and [d][] and [text][x]\n";
        let config = config_with(MD052ReferenceLinksImagesTable {
            shortcut_syntax: true,
            ..Default::default()
        });
        // markdownlint reports `[a]`, `[b][c]` and `[d][]`, and not `[text][x]` — `x` is ignored.
        assert_eq!(
            vec![(3, 1), (3, 9), (3, 20)],
            positions_with(config, source)
        );
        // The same document with the default leaves the lone label alone.
        assert_eq!(vec![(3, 9), (3, 20)], positions(source));
    }

    #[test]
    fn ignored_labels_replace_the_default() {
        let source = "# H\n\n[a] and [b][c] and [d][] and [text][x]\n";
        let config = config_with(MD052ReferenceLinksImagesTable {
            ignored_labels: vec!["c".to_string(), "d".to_string()],
            ..MD052ReferenceLinksImagesTable {
                shortcut_syntax: false,
                ignored_labels: vec![],
            }
        });
        // markdownlint reports only `[text][x]`: setting `ignored_labels` drops the default `x`.
        assert_eq!(vec![(3, 30)], positions_with(config, source));
    }

    #[test]
    fn a_document_without_references_is_quiet() {
        assert_eq!(0, positions("# H\n\n[a](http://x) and plain text\n").len());
    }
}
