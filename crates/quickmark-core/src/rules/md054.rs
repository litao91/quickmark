use serde::Deserialize;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{
        closing_bracket, default_on, ellipsify, label_span, Context, Rule, RuleLinter, RuleType,
    },
};

// MD054-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD054LinkImageStyleTable {
    #[serde(default = "default_on")]
    pub autolink: bool,
    #[serde(default = "default_on")]
    pub inline: bool,
    #[serde(default = "default_on")]
    pub full: bool,
    #[serde(default = "default_on")]
    pub collapsed: bool,
    #[serde(default = "default_on")]
    pub shortcut: bool,
    #[serde(default = "default_on")]
    pub url_inline: bool,
}

impl Default for MD054LinkImageStyleTable {
    fn default() -> Self {
        Self {
            autolink: true,
            inline: true,
            full: true,
            collapsed: true,
            shortcut: true,
            url_inline: true,
        }
    }
}

/// How a link or image is written, which is the only thing this rule asks about.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `<https://x.com>` or `<a@b.com>`
    Autolink,
    /// `[a](https://x.com)`
    Inline,
    /// `[a][ref]`
    Full,
    /// `[a][]`
    Collapsed,
    /// `[a]`
    Shortcut,
}

impl Style {
    fn is_reference(self) -> bool {
        matches!(self, Self::Full | Self::Collapsed | Self::Shortcut)
    }
}

/// Whether a link is a violation already, or one only if markdownlint finds its definition — which
/// is not known until the whole document has been seen.
#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    No,
    Yes,
    IfDefined,
}

struct Found {
    verdict: Verdict,
    /// The link's own span.
    from: usize,
    to: usize,
    /// The span markdownlint looks up in its definitions map: the reference string of a full
    /// reference, the label of a collapsed or shortcut one.
    key: (usize, usize),
}

/// MD054 - Link and image style
///
/// Reports every link and image written in a style the configuration disallows.
pub(crate) struct MD054Linter {
    context: Rc<Context>,
    found: Vec<Found>,
    /// The normalized label of every definition in the document, and whether it has a destination.
    /// The first of each label wins, as it does for the parser.
    definitions: HashMap<String, bool>,
}

impl MD054Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            found: Vec::new(),
            definitions: HashMap::new(),
        }
    }

    fn config(&self) -> &MD054LinkImageStyleTable {
        &self.context.config.linters.settings.link_image_style
    }

    /// Whether any style is disallowed at all, which is the only case markdownlint looks at the
    /// document for.
    fn checking(&self) -> bool {
        let config = self.config();
        !(config.autolink
            && config.inline
            && config.full
            && config.collapsed
            && config.shortcut
            && config.url_inline)
    }

    fn collect(&mut self, inline: Node) {
        let found = {
            let source = self.context.document_content.borrow();
            self.links(inline, &source)
        };
        self.found.extend(found);
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch. Pre-order, so a link holding an image is reported before the image.
    fn links(&self, inline: Node, source: &str) -> Vec<Found> {
        let mut found = Vec::new();
        let mut cursor = inline.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if matches!(node.kind(), "link" | "image") {
                found.extend(self.classify(node, source));
            }
            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
                    return found;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return found;
                }
                depth -= 1;
            }
        }
    }

    /// Reads a link's style off the source and settles everything about it that does not depend on
    /// the document's definitions.
    ///
    /// The style comes from the bytes rather than from the parser because comrak resolves a
    /// reference and keeps only the destination, which loses the distinction between `[a][ref]`,
    /// `[a][]` and `[a]`.
    fn classify(&self, node: Node, source: &str) -> Option<Found> {
        let from = node.start_byte();
        // An autolink's protocol or email token is never empty, so there is nothing to look up.
        if source.as_bytes().get(from) == Some(&b'<') {
            return Some(Found {
                verdict: self.verdict(Style::Autolink, node, source),
                from,
                to: node.end_byte(),
                key: (from, from),
            });
        }

        let label = label_span(node, source)?;
        let bytes = source.as_bytes();
        let after = label.to + 1;
        // An inline link with no destination at all — `[a]()`, `[a]( )`, `[a](<>)` — has no
        // destination string for micromark either, so markdownlint falls through to its reference
        // branch and treats it as a shortcut on the label.
        let empty_destination = node
            .link_target()
            .is_none_or(|target| target.url.is_empty());
        let (style, key) = match bytes.get(after) {
            Some(b'(') if !empty_destination => (Style::Inline, (label.from, label.to)),
            Some(b'[') => match closing_bracket(bytes, after) {
                // An empty reference group means the label is the reference.
                Some(close) if close == after + 1 => (Style::Collapsed, (label.from, label.to)),
                Some(close) => (Style::Full, (after + 1, close)),
                None => (Style::Shortcut, (label.from, label.to)),
            },
            _ => (Style::Shortcut, (label.from, label.to)),
        };
        let verdict = match self.verdict(style, node, source) {
            // markdownlint looks the raw reference string up in a map keyed by the *normalized*
            // definition label, so one that matches only after normalizing finds nothing.
            Verdict::Yes if style.is_reference() => Verdict::IfDefined,
            verdict => verdict,
        };
        Some(Found {
            verdict,
            from,
            to: node.end_byte(),
            key,
        })
    }

    fn verdict(&self, style: Style, node: Node, source: &str) -> Verdict {
        let config = self.config();
        let target = node.link_target();
        let url = target.map(|target| target.url.as_str()).unwrap_or("");
        if match style {
            Style::Autolink => !config.autolink,
            Style::Inline => !config.inline,
            Style::Full => !config.full,
            Style::Collapsed => !config.collapsed,
            Style::Shortcut => !config.shortcut,
        } {
            return Verdict::Yes;
        }
        // An inline link whose text is its own destination is a style of its own, `url_inline`,
        // which markdownlint only offers to rewrite as an autolink — so it takes one that could be
        // an autolink, and one with no title to lose.
        if style == Style::Inline
            && !config.url_inline
            && config.autolink
            && node.kind() != "image"
            && target.is_some_and(|target| target.title.is_empty())
            && autolink_able(url)
            && label_span(node, source)
                .is_some_and(|label| source.get(label.from..label.to) == Some(url))
        {
            return Verdict::Yes;
        }
        Verdict::No
    }

    fn violation(&self, found: &Found, source: &str) -> RuleViolation {
        // markdownlint quotes a link only as far as its first line break.
        let text = source
            .get(found.from..found.to)
            .unwrap_or_default()
            .split(['\r', '\n'])
            .next()
            .unwrap_or_default();
        let start = self.context.point_at(found.from);
        let end_byte = {
            let lines = self.context.lines.borrow();
            let line_end = self.context.line_start_byte(start.row) + lines[start.row].len();
            found.to.min(line_end)
        };
        RuleViolation::new(
            &MD054,
            format!(
                "{} [Context: \"{}\"]",
                MD054.description,
                ellipsify(text, false, false)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: found.from,
                end_byte,
                start_point: start,
                end_point: self.context.point_at(end_byte),
            }),
        )
    }

    /// Records a link reference definition. The node's shape is the parser's business, so this reads
    /// the label and whatever follows it off the source.
    fn add_definition(&mut self, node: Node) {
        let definition = {
            let source = self.context.document_content.borrow();
            definition(node, &source)
        };
        if let Some((label, has_destination)) = definition {
            self.definitions.entry(label).or_insert(has_destination);
        }
    }
}

/// A definition's normalized label and whether it has a destination, which is all markdownlint's
/// reference branch asks of it.
///
/// markdownlint reads the destination through micromark's `definitionDestinationRaw`, which only
/// exists for one written bare — so a destination in angle brackets comes back empty however much
/// is inside the brackets, and a reference to it is never a violation.
fn definition(node: Node, source: &str) -> Option<(String, bool)> {
    let bytes = source.as_bytes();
    let start = node.start_byte();
    let indent = bytes[start..]
        .iter()
        .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
        .take(3)
        .count();
    let open = start + indent;
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let close = closing_bracket(bytes, open)?;
    // markdownlint's `normalizeReference`: lower case, trimmed, one space per whitespace run.
    let label = source
        .get(open + 1..close)?
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let destination = source
        .get(close + 1..node.end_byte())?
        .strip_prefix(':')?
        .trim_start();
    let has_destination = !destination.is_empty() && !destination.starts_with('<');
    Some((label, has_destination))
}

/// Whether a destination is something an autolink's angle brackets could hold, which is
/// markdownlint's `autolinkAble`: JavaScript's `new URL` accepts it, and it has none of the three
/// characters that cannot appear inside one.
fn autolink_able(destination: &str) -> bool {
    let Some(colon) = destination.find(':') else {
        return false;
    };
    let scheme = &destination[..colon];
    let valid = scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
    valid && !destination.contains([' ', '<', '>'])
}

impl RuleLinter for MD054Linter {
    fn feed(&mut self, node: &Node) {
        if !self.checking() {
            return;
        }
        match node.kind() {
            "inline" => self.collect(*node),
            "link_reference_definition" => self.add_definition(*node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        let source = self.context.document_content.borrow();
        self.found
            .iter()
            .filter(|found| {
                found.verdict == Verdict::Yes
                    || (found.verdict == Verdict::IfDefined
                        && source
                            .get(found.key.0..found.key.1)
                            // A reference is a violation only if markdownlint finds its definition
                            // and that definition has a destination.
                            .and_then(|key| self.definitions.get(key))
                            .is_some_and(|&has_destination| has_destination))
            })
            .map(|found| self.violation(found, &source))
            .collect()
    }
}

pub const MD054: Rule = Rule {
    id: "MD054",
    aliases: &["link-image-style"],
    tags: &["links", "images"],
    description: "Link and image style",
    rule_type: RuleType::Token,
    required_nodes: &["inline", "link_reference_definition"],
    new_linter: |context| Box::new(MD054Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD054LinkImageStyleTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    /// One setting is turned off at a time and the rest left on, which is the only way the rule does
    /// any work at all.
    const SETTINGS: [&str; 6] = [
        "autolink",
        "inline",
        "full",
        "collapsed",
        "shortcut",
        "url_inline",
    ];

    /// A report: the 1-based line and column of the link, and the text markdownlint quotes.
    type Report = (usize, usize, &'static str);
    type Found = (usize, usize, String);
    /// A case's name, its document, and the settings that make markdownlint report it, with what.
    type Expected = (&'static str, &'static [Report]);
    type Case = (&'static str, &'static str, &'static [Expected]);

    fn test_config(off: &str) -> crate::config::QuickmarkConfig {
        let allowed = |name: &str| name != off;
        test_config_with_settings(
            vec![("link-image-style", RuleSeverity::Error)],
            LintersSettingsTable {
                link_image_style: MD054LinkImageStyleTable {
                    autolink: allowed("autolink"),
                    inline: allowed("inline"),
                    full: allowed("full"),
                    collapsed: allowed("collapsed"),
                    shortcut: allowed("shortcut"),
                    url_inline: allowed("url_inline"),
                },
                ..Default::default()
            },
        )
    }

    fn owned(reports: &[Report]) -> Vec<Found> {
        reports
            .iter()
            .map(|&(line, column, context)| (line, column, context.to_string()))
            .collect()
    }

    fn reports(input: &str, off: &str) -> Vec<Found> {
        let config = test_config(off);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let mut found: Vec<Found> = linter
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
            .collect();
        found.sort();
        found
    }

    /// Every expectation measured against markdownlint-cli2 v0.23.3, across all six settings.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            (
                "an inline link",
                "[a](https://x.com)\n",
                &[("inline", &[(1, 1, "[a](https://x.com)")])],
            ),
            (
                "an inline image",
                "![a](x.png)\n",
                &[("inline", &[(1, 1, "![a](x.png)")])],
            ),
            (
                "a full reference link",
                "[a][ref]\n\n[ref]: https://x.com\n",
                &[("full", &[(1, 1, "[a][ref]")])],
            ),
            (
                "a collapsed reference link",
                "[a][]\n\n[a]: https://x.com\n",
                &[("collapsed", &[(1, 1, "[a][]")])],
            ),
            (
                "a shortcut reference link",
                "[a]\n\n[a]: https://x.com\n",
                &[("shortcut", &[(1, 1, "[a]")])],
            ),
            (
                "a url autolink",
                "<https://x.com>\n",
                &[("autolink", &[(1, 1, "<https://x.com>")])],
            ),
            (
                "an email autolink",
                "<a@b.com>\n",
                &[("autolink", &[(1, 1, "<a@b.com>")])],
            ),
            (
                "a url inline link",
                "[https://x.com](https://x.com)\n",
                &[
                    ("inline", &[(1, 1, "[https://x.com](https://x.com)")]),
                    ("url_inline", &[(1, 1, "[https://x.com](https://x.com)")]),
                ],
            ),
            ("a full reference with no definition", "[a][missing]\n", &[]),
            (
                "a full reference image",
                "![a][ref]\n\n[ref]: x.png\n",
                &[("full", &[(1, 1, "![a][ref]")])],
            ),
            (
                "a collapsed reference image",
                "![a][]\n\n[a]: x.png\n",
                &[("collapsed", &[(1, 1, "![a][]")])],
            ),
            (
                "a shortcut reference image",
                "![a]\n\n[a]: x.png\n",
                &[("shortcut", &[(1, 1, "![a]")])],
            ),
            (
                "an inline link with a title",
                "[a](https://x.com \"t\")\n",
                &[("inline", &[(1, 1, "[a](https://x.com \"t\")")])],
            ),
            (
                "a url inline link with a title",
                "[https://x.com](https://x.com \"t\")\n",
                &[("inline", &[(1, 1, "[https://x.com](https://x.com ...")])],
            ),
            (
                "an inline link that is not a url",
                "[not-a-url](not-a-url)\n",
                &[("inline", &[(1, 1, "[not-a-url](not-a-url)")])],
            ),
            (
                "a label holding brackets",
                "[a [b] c](https://x.com)\n",
                &[("inline", &[(1, 1, "[a [b] c](https://x.com)")])],
            ),
            (
                "a label holding escaped brackets",
                "[a \\[b\\] c](https://x.com)\n",
                &[("inline", &[(1, 1, "[a \\[b\\] c](https://x.com)")])],
            ),
            ("inside a code span", "`[a](https://x.com)`\n", &[]),
            (
                "a destination in angle brackets",
                "[a](<https://x.com>)\n",
                &[("inline", &[(1, 1, "[a](<https://x.com>)")])],
            ),
            (
                "a destination holding parentheses",
                "[a](https://(x).com)\n",
                &[("inline", &[(1, 1, "[a](https://(x).com)")])],
            ),
            (
                "a label that is a bracket group",
                "[[a]](https://x.com)\n",
                &[("inline", &[(1, 1, "[[a]](https://x.com)")])],
            ),
            (
                "two shortcut references",
                "[a] [b]\n\n[a]: /1\n[b]: /2\n",
                &[("shortcut", &[(1, 1, "[a]"), (1, 5, "[b]")])],
            ),
            (
                "a chain of bracket groups",
                "[a][b][c]\n\n[b]: /1\n[c]: /2\n",
                &[
                    ("full", &[(1, 1, "[a][b]")]),
                    ("shortcut", &[(1, 7, "[c]")]),
                ],
            ),
            (
                "a nested bracket group with no definition",
                "![[note]]\n",
                &[],
            ),
            ("a bare url", "text https://x.com text\n", &[]),
            (
                "in a heading",
                "# [a](https://x.com)\n",
                &[("inline", &[(1, 3, "[a](https://x.com)")])],
            ),
            (
                "in a table cell",
                "| x |\n| - |\n| [a](https://x.com) |\n",
                &[("inline", &[(3, 3, "[a](https://x.com)")])],
            ),
            (
                "in a block quote",
                "> [a](https://x.com)\n",
                &[("inline", &[(1, 3, "[a](https://x.com)")])],
            ),
            (
                "in a list item",
                "- [a](https://x.com)\n",
                &[("inline", &[(1, 3, "[a](https://x.com)")])],
            ),
            (
                "an inline link with a relative destination",
                "[a](b)\n",
                &[("inline", &[(1, 1, "[a](b)")])],
            ),
            ("a collapsed reference with no definition", "[a][]\n", &[]),
            ("a shortcut reference with no definition", "[a]\n", &[]),
            (
                "an inline link with an empty label",
                "[](https://x.com)\n",
                &[("inline", &[(1, 1, "[](https://x.com)")])],
            ),
            (
                "an inline link around an inline image",
                "[![img](x.png)](https://x.com)\n",
                &[(
                    "inline",
                    &[
                        (1, 1, "[![img](x.png)](https://x.com)"),
                        (1, 2, "![img](x.png)"),
                    ],
                )],
            ),
            (
                "two inline links",
                "[a](https://x.com) and [b](https://y.com)\n",
                &[(
                    "inline",
                    &[(1, 1, "[a](https://x.com)"), (1, 24, "[b](https://y.com)")],
                )],
            ),
            ("an inline link with no destination", "[a]()\n", &[]),
            (
                "an inline link with an empty angle bracket destination",
                "[a](<>)\n",
                &[],
            ),
            (
                "an inline link with an empty title",
                "[a](b \"\")\n",
                &[("inline", &[(1, 1, "[a](b \"\")")])],
            ),
            ("an inline link with a blank destination", "[a]( )\n", &[]),
            (
                "an inline link with an angle bracket destination",
                "[a](<https://x.com>)\n",
                &[("inline", &[(1, 1, "[a](<https://x.com>)")])],
            ),
            (
                "a url inline link with an angle bracket destination",
                "[https://x.com](<https://x.com>)\n",
                &[
                    ("inline", &[(1, 1, "[https://x.com](<https://x.com...")]),
                    ("url_inline", &[(1, 1, "[https://x.com](<https://x.com...")]),
                ],
            ),
            (
                "a full reference whose case differs from its definition",
                "[a][REF]\n\n[ref]: https://x.com\n",
                &[],
            ),
            (
                "a collapsed reference whose case differs from its definition",
                "[A][]\n\n[a]: https://x.com\n",
                &[],
            ),
            (
                "a shortcut reference to a definition with no destination",
                "[a]: \n\n[a]\n",
                &[],
            ),
            (
                "an inline link with a mailto destination",
                "[a](mailto:x@y.com)\n",
                &[("inline", &[(1, 1, "[a](mailto:x@y.com)")])],
            ),
            (
                "an email inline link with a mailto destination",
                "[x@y.com](mailto:x@y.com)\n",
                &[("inline", &[(1, 1, "[x@y.com](mailto:x@y.com)")])],
            ),
            (
                "an inline link before a hard line break",
                "[a](https://x.com)\\\nb\n",
                &[("inline", &[(1, 1, "[a](https://x.com)")])],
            ),
            (
                "an inline link inside strong emphasis",
                "**[a](https://x.com)**\n",
                &[("inline", &[(1, 3, "[a](https://x.com)")])],
            ),
            (
                "an inline link around an emphasis",
                "[*a*](https://x.com)\n",
                &[("inline", &[(1, 1, "[*a*](https://x.com)")])],
            ),
            (
                "an inline link and a collapsed reference",
                "[a](https://x.com \"t\")[b][]\n\n[b]: /u\n",
                &[
                    ("inline", &[(1, 1, "[a](https://x.com \"t\")")]),
                    ("collapsed", &[(1, 23, "[b][]")]),
                ],
            ),
            (
                "an empty inline link with a definition",
                "[a]()\n\n[a]: /u\n",
                &[("shortcut", &[(1, 1, "[a]()")])],
            ),
            (
                "an empty inline image with a definition",
                "![a]()\n\n[a]: x.png\n",
                &[("shortcut", &[(1, 1, "![a]()")])],
            ),
            (
                "an angle bracket inline link with a definition",
                "[a](<>)\n\n[a]: /u\n",
                &[("shortcut", &[(1, 1, "[a](<>)")])],
            ),
            (
                "a full reference to an empty angle bracket destination",
                "[a][ref]\n\n[ref]: <>\n",
                &[],
            ),
            (
                "an empty inline link with an empty angle bracket destination",
                "[a]()\n\n[a]: <>\n",
                &[],
            ),
            (
                "a full reference to an angle bracket destination",
                "[a][ref]\n\n[ref]: <b>\n",
                &[],
            ),
        ];
        for (name, input, expected) in cases {
            for off in SETTINGS {
                let want = expected
                    .iter()
                    .find(|(setting, _)| *setting == off)
                    .map(|&(_, reports)| reports)
                    .unwrap_or(&[]);
                assert_eq!(owned(want), reports(input, off), "{name}, {off} off");
            }
        }
    }

    #[test]
    fn everything_allowed_checks_nothing() {
        let config = test_config_with_settings(
            vec![("link-image-style", RuleSeverity::Error)],
            LintersSettingsTable::default(),
        );
        let input = "[a](b)\n\n[c][d]\n\n[d]: /e\n\n<f>\n";
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        assert_eq!(0, linter.analyze().len());
    }

    /// markdownlint quotes a link only as far as its first line break, and gives it no range at all
    /// — so markdownlint-cli2 prints no column. The report here keeps the line and the truncated
    /// context, and its range stops at the end of that line.
    #[test]
    fn a_link_spanning_lines_is_reported_on_its_first_line() {
        let input = "[a\nb](https://x.com)\n";
        assert_eq!(vec![(1, 1, "[a".to_string())], reports(input, "inline"));

        let config = test_config("inline");
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        let range = &violations[0].location().range;
        assert_eq!(
            (0, 0, 0, 2),
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            )
        );
    }
}
