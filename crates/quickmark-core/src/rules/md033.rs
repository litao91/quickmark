use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::{collections::HashSet, rc::Rc};

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{Rule, RuleType},
};

// MD033-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
pub struct MD033InlineHtmlTable {
    #[serde(default)]
    pub allowed_elements: Vec<String>,
}

/// CommonMark's open and close tag grammar, which is what micromark tokenizes and therefore what
/// markdownlint can see. Being strict about it is the point: `(?:[\s/][^>]*)?>` also matched
/// `<br \>`, and a backslash is not an attribute name, so micromark leaves that as literal text and
/// markdownlint never reports it — 132 false positives on the vault corpus, all from HTML tables
/// written with `<br \>` as a line break.
///
/// Requiring whitespace or `/` or `>` straight after the tag name is also what keeps autolinks out:
/// `<https://example.com>` and `<foo@example.com>` are links, not HTML. Tag names may contain
/// hyphens, as in `<ne-text>`.
static HTML_TAG_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r#"<(/?)"#,
        r#"([a-zA-Z][a-zA-Z0-9-]*)"#,
        // attribute = whitespace+ name ( "=" whitespace* value )?
        r#"(?:[ \t\f\n\r]+[a-zA-Z:_][a-zA-Z0-9_.:-]*"#,
        r#"(?:[ \t\f\n\r]*=[ \t\f\n\r]*(?:[^ \t\f\n\r"'=<>`]+|'[^']*'|"[^"]*"))?"#,
        r#")*"#,
        r#"[ \t\f\n\r]*/?>"#,
    ))
    .expect("Invalid HTML tag regex")
});

pub(crate) struct MD033Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    allowed_elements: HashSet<String>,
}

impl MD033Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // Pre-process allowed elements into a HashSet for O(1) lookups
        let allowed_elements: HashSet<String> = context
            .config
            .linters
            .settings
            .inline_html
            .allowed_elements
            .iter()
            .map(|element| element.to_lowercase())
            .collect();

        Self {
            context,
            violations: Vec::new(),
            allowed_elements,
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch, and reports the `html_inline` nodes in it.
    ///
    /// A code span, inline math and an autolink are all leaves, so nothing inside one can be
    /// mistaken for a tag: `` `Foo<T>::f()` `` and `$A<B>C<D$` are quiet here exactly as they are
    /// in micromark, and no masking regex is needed to make that so.
    fn check_inline(&mut self, root: Node) {
        let mut tags = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "html_inline" {
                tags.push(node);
            }
            for index in 0..node.child_count() {
                if let Some(child) = node.child(index) {
                    stack.push(child);
                }
            }
        }
        // The walk pops siblings in reverse; violations come out in document order.
        tags.sort_unstable_by_key(|tag| tag.start_byte());

        let mut found = Vec::new();
        {
            let source = self.context.document_content.borrow();
            for tag in tags {
                let range = tag.range();
                let text = source[range.start_byte..range.end_byte].to_string();
                // `getHtmlTagInfo` is markdownlint's own test, `/^<([^!>][^/\s>]*)/`: a comment, a
                // CDATA section or a declaration has no name and is not an element, and a closing
                // tag is not reported.
                let Some(name) = html_tag_info(&text).map(str::to_string) else {
                    continue;
                };
                found.push((range.start_byte, name, text));
            }
        }

        for (start, name, text) in found {
            self.report(start, &name, &text);
        }
    }

    /// Finds the tags in an HTML block's raw text.
    ///
    /// micromark re-tokenizes the content of every HTML block type as inline markdown except a
    /// comment's, which stays raw — so `<!-- <link> -->` holds no tags at all while
    /// `<div>\n<p>x</p>\n</div>` holds four, of which the two closing ones are not reported. A
    /// comment, a CDATA section, a declaration and a processing instruction are each a single
    /// token here too, and only the last has a name markdownlint reports.
    ///
    /// That re-tokenization is the one part this has to redo by hand, because comrak's `html_block`
    /// is a leaf: code spans are located and skipped, since a `` `<b>` `` inside an HTML block is
    /// code to micromark and so is not an element. Inline math is not; `$A<B>C$` inside an HTML
    /// block reports `B` here and nothing in markdownlint.
    fn check_html_block(&mut self, node: Node) {
        let base = node.start_byte();
        let mut found = Vec::new();
        {
            let source = self.context.document_content.borrow();
            let text = &source[base..node.end_byte()];
            let code = code_spans(text);
            let mut index = 0;
            while let Some(offset) = text[index..].find('<') {
                let start = index + offset;
                let rest = &text[start..];
                // A code span is code, so a `<` inside one opens nothing.
                if code.iter().any(|&(from, to)| from <= start && start < to) {
                    index = start + 1;
                    continue;
                }
                if let Some((length, name)) = raw_construct(rest) {
                    if let Some(name) = name {
                        found.push((base + start, name.to_string(), rest[..length].to_string()));
                    }
                    index = start + length;
                    continue;
                }
                match HTML_TAG_REGEX.find(rest) {
                    Some(found_tag) if found_tag.start() == 0 => {
                        let tag = &rest[..found_tag.end()];
                        if !tag.starts_with("</") {
                            let name = HTML_TAG_REGEX
                                .captures(tag)
                                .and_then(|caps| caps.get(2))
                                .map_or("", |name| name.as_str());
                            found.push((base + start, name.to_string(), tag.to_string()));
                        }
                        index = start + found_tag.end();
                    }
                    _ => index = start + 1,
                }
            }
        }

        for (start, name, text) in found {
            self.report(start, &name, &text);
        }
    }

    fn report(&mut self, start: usize, name: &str, text: &str) {
        if self.allowed_elements.contains(&name.to_lowercase()) {
            return;
        }
        // markdownlint's range stops at the tag's first line ending, so a tag written across
        // several lines is underlined only as far as its opening line goes.
        let end = start + text.find(['\n', '\r']).unwrap_or(text.len());
        let range = range_from_node_range(&crate::ast::NodeRange {
            start_byte: start,
            end_byte: end,
            start_point: self.context.point_at(start),
            end_point: self.context.point_at(end),
        });
        self.violations.push(RuleViolation::new(
            &MD033,
            format!("Inline HTML [Element: {name}]"),
            self.context.file_path.clone(),
            range,
        ));
    }
}

/// The byte spans of the code spans in `text`, by CommonMark's rule: a run of N backticks is closed
/// by the next run of exactly N, and an opener with no such close is literal text.
///
/// Pairing stops at a blank line, because a code span may not cross one. micromark gets that for
/// free — it re-tokenizes an HTML block's content as *flow*, so blank lines split it into separate
/// paragraphs before inline parsing ever runs — and without the same limit a stray backtick early in
/// a long `<pre>` block swallows everything after it.
fn code_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut base = 0;
    for chunk in text.split("\n\n") {
        spans.extend(
            code_spans_in(chunk)
                .into_iter()
                .map(|(from, to)| (from + base, to + base)),
        );
        base += chunk.len() + 2;
    }
    spans
}

fn code_spans_in(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let open = index;
        while index < bytes.len() && bytes[index] == b'`' {
            index += 1;
        }
        let width = index - open;
        let mut probe = index;
        while probe < bytes.len() {
            if bytes[probe] != b'`' {
                probe += 1;
                continue;
            }
            let close = probe;
            while probe < bytes.len() && bytes[probe] == b'`' {
                probe += 1;
            }
            if probe - close == width {
                spans.push((open, probe));
                index = probe;
                break;
            }
        }
    }
    spans
}

/// The element name of an inline HTML tag, or `None` for a closing tag, a comment, a CDATA section,
/// a processing instruction or a declaration. markdownlint's own test is `/^<([^!>][^/\s>]*)/`,
/// which takes a leading `/` as part of the name and reads it as the close — and a leading `?` as
/// part of the name too, which is how `<?php … ?>` comes to be an element called `?php`.
fn html_tag_info(tag: &str) -> Option<&str> {
    let rest = tag.strip_prefix('<')?;
    let first = rest.as_bytes().first()?;
    if *first == b'!' || *first == b'>' {
        return None;
    }
    let name_end = rest[1..]
        .find(|c: char| c == '/' || c.is_whitespace() || c == '>')
        .map_or(rest.len(), |offset| offset + 1);
    let name = &rest[..name_end];
    if name.starts_with('/') {
        return None;
    }
    Some(name)
}

/// A construct micromark tokenizes whole, so nothing inside it is a tag: `(length, name to report)`.
/// Only a processing instruction has one. An unterminated construct runs to the end of the text.
fn raw_construct(text: &str) -> Option<(usize, Option<&str>)> {
    let bytes = text.as_bytes();
    let (marker, terminator) = if text.starts_with("<!--") {
        ("<!--", "-->")
    } else if text.starts_with("<![CDATA[") {
        ("<![CDATA[", "]]>")
    } else if text.starts_with("<?") {
        ("<?", "?>")
    } else if bytes.get(1) == Some(&b'!')
        && bytes.get(2).is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        ("<!", ">")
    } else {
        return None;
    };
    let length = text[marker.len()..]
        .find(terminator)
        .map_or(text.len(), |offset| {
            marker.len() + offset + terminator.len()
        });
    let name = text.starts_with("<?").then(|| {
        let rest = &text[1..];
        let end = rest
            .find(|c: char| c == '/' || c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        &rest[..end]
    });
    Some((length, name))
}

impl RuleLinter for MD033Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "inline" => self.check_inline(*node),
            "html_block" => self.check_html_block(*node),
            _ => (),
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD033: Rule = Rule {
    id: "MD033",
    alias: "no-inline-html",
    tags: &["html"],
    description: "Inline HTML",
    rule_type: RuleType::Token,
    required_nodes: &["inline", "html_block"],
    new_linter: |context| Box::new(MD033Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD033InlineHtmlTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::{test_config_with_rules, test_config_with_settings};

    /// One reported tag: `(line, column, width, element)`, the first three 1-based. The width runs
    /// to the tag's first line ending, which is what markdownlint's `errorRange` covers, and the
    /// element keeps the case it was written in.
    type Tag<'a> = (usize, usize, usize, &'a str);

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-inline-html", RuleSeverity::Error)])
    }

    fn tags_with(
        config: crate::config::QuickmarkConfig,
        source: &str,
    ) -> Vec<(usize, usize, usize, String)> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                let element = violation
                    .message()
                    .split_once("[Element: ")
                    .map_or("", |(_, rest)| rest.trim_end_matches(']'))
                    .to_string();
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                    element,
                )
            })
            .collect()
    }

    fn tags(source: &str) -> Vec<(usize, usize, usize, String)> {
        tags_with(test_config(), source)
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output, run with
    /// only `no-inline-html` enabled: its line, its `errorRange` column and length, and the element
    /// name from its `errorDetail`.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so the one case with a multi-byte
    /// character before a tag is asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &[Tag<'static>])] = &[
        ("a <div> b\n", &[(1, 3, 5, "div")]),
        ("a </div> b\n", &[]),
        ("a <br/> b\n", &[(1, 3, 5, "br")]),
        ("a <img src=\"x\" alt='y'> b\n", &[(1, 3, 21, "img")]),
        ("a <!-- <link rel=x> --> b\n", &[]),
        ("<!-- <link rel=x> -->\n", &[]),
        (
            "<div>\n<p>x</p>\n</div>\n",
            &[(1, 1, 5, "div"), (2, 1, 3, "p")],
        ),
        ("<!--\n<link rel=x>\n-->\n", &[]),
        ("a `<div>` b\n", &[]),
        ("```\n<div>\n```\n", &[]),
        ("text\n\n    <div>\n", &[]),
        ("- assume $A<B>C<D$\n", &[]),
        ("$$\nA<B>C\n$$\n", &[]),
        ("see <https://example.com> and <a@b.com>\n", &[]),
        ("# <div> heading\n", &[(1, 3, 5, "div")]),
        ("| a |\n|---|\n| <b>x</b> |\n", &[(3, 3, 3, "b")]),
        ("> <div> x\n", &[(1, 3, 5, "div")]),
        ("- <div> x\n", &[(1, 3, 5, "div")]),
        ("[<b>x</b>](http://y)\n", &[(1, 2, 3, "b")]),
        ("a <br \\> b\n", &[]),
        ("a<b>c\n", &[(1, 2, 3, "b")]),
        ("a <DIV> b\n", &[(1, 3, 5, "DIV")]),
        ("a <ne-text> b\n", &[(1, 3, 9, "ne-text")]),
        (
            "a <b>x</b> <i>y</i> c\n",
            &[(1, 3, 3, "b"), (1, 12, 3, "i")],
        ),
        ("a <div\n  class='x'> b\n", &[(1, 3, 4, "div")]),
        ("a < b > c and 1<2>3\n", &[]),
        ("<!DOCTYPE html>\n<div>x</div>\n", &[(2, 1, 5, "div")]),
        ("a <![CDATA[ x ]]> b\n", &[]),
        ("a <?php echo 1; ?> b\n", &[(1, 3, 16, "?php")]),
        (
            "<script>\nvar x = '<div>';\n</script>\n",
            &[(1, 1, 8, "script"), (2, 10, 5, "div")],
        ),
        ("<?php\n$x = '<div>';\n?>\n", &[(1, 1, 5, "?php")]),
        ("<![CDATA[\n<div>\n]]>\n", &[]),
        (
            "<div>\ntext <b>y</b>\n\n",
            &[(1, 1, 5, "div"), (2, 6, 3, "b")],
        ),
        ("a <!--\n<div>\n--> b\n", &[(2, 1, 5, "div")]),
        (
            "<span class='x'>\ntext <b>y</b>\n\n",
            &[(1, 1, 16, "span"), (2, 6, 3, "b")],
        ),
        ("<div>\n<!-- <b>x</b> -->\n</div>\n", &[(1, 1, 5, "div")]),
        ("<div>\n\n<p>x</p>\n", &[(1, 1, 5, "div"), (3, 1, 3, "p")]),
        ("no html here at all\n", &[]),
        ("a <div>\n", &[(1, 3, 5, "div")]),
        (
            "text <sub>x</sub> and <sup>y</sup>\n",
            &[(1, 6, 5, "sub"), (1, 23, 5, "sup")],
        ),
        ("<div>\n`<b>`\n</div>\n", &[(1, 1, 5, "div")]),
        (
            "<div>\n`<b>` and <i>x</i>\n</div>\n",
            &[(1, 1, 5, "div"), (2, 11, 3, "i")],
        ),
        ("<pre class=x>\ntext `<T>` more\n", &[(1, 1, 13, "pre")]),
        ("<div>\n``a <b>`` c\n</div>\n", &[(1, 1, 5, "div")]),
        (
            "<div>\nx `y\n\nz <b> w\n</div>\n",
            &[(1, 1, 5, "div"), (4, 3, 3, "b")],
        ),
        (
            "<pre>\n`a` <b>\n\n`c` <i>\n</pre>\n",
            &[(1, 1, 5, "pre"), (2, 5, 3, "b"), (4, 5, 3, "i")],
        ),
        (
            "<div>\n`unclosed <b>\n</div>\n",
            &[(1, 1, 5, "div"), (2, 11, 3, "b")],
        ),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            let found = tags(source);
            let reported: Vec<Tag<'_>> = found
                .iter()
                .map(|(line, column, width, element)| (*line, *column, *width, element.as_str()))
                .collect();
            assert_eq!(expected, reported.as_slice(), "source {source:?}");
        }
    }

    /// A tag after a multi-byte character. markdownlint counts UTF-16 units, so its column is
    /// smaller than the byte-based one quickmark reports. That is the byte-column convention every
    /// rule shares, not an MD033 difference.
    #[test]
    fn positions_count_bytes() {
        // markdownlint: [(1, 3, 5, "div")]
        assert_eq!(vec![(1, 5, 5, "div".to_string())], tags("你 <div> 好\n"));
    }

    /// micromark re-tokenizes a non-comment HTML block's content as inline markdown, and this scan
    /// redoes only the code-span part of that: inline math inside an HTML block still hides its
    /// contents from markdownlint and not from here. markdownlint reports only `div`.
    #[test]
    fn inline_math_inside_an_html_block_is_not_masked() {
        assert_eq!(
            vec![(1, 1, 5, "div".to_string()), (2, 3, 3, "B".to_string())],
            tags("<div>\n$A<B>C$\n</div>\n")
        );
    }

    #[test]
    fn allowed_elements_is_case_insensitive() {
        // markdownlint with `allowed_elements: ["div"]` and with `["DIV"]` both report only `span`.
        for allowed in [vec!["div".to_string()], vec!["DIV".to_string()]] {
            let config = test_config_with_settings(
                vec![("no-inline-html", RuleSeverity::Error)],
                LintersSettingsTable {
                    inline_html: MD033InlineHtmlTable {
                        allowed_elements: allowed,
                    },
                    ..Default::default()
                },
            );
            let found = tags_with(config, "a <div> b <span>c</span> <DIV>d</DIV>\n");
            let elements: Vec<&str> = found.iter().map(|tag| tag.3.as_str()).collect();
            assert_eq!(vec!["span"], elements);
        }
    }

    #[test]
    fn a_document_without_html_is_quiet() {
        assert_eq!(0, tags("plain *text* with `code` and $math$\n").len());
    }
}
