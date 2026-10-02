use std::collections::HashMap;
use std::rc::Rc;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

// MD051-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
pub struct MD051LinkFragmentsTable {
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub ignored_pattern: String,
}

/// GitHub's line fragments, which point into a file rather than at a heading and so are always
/// valid: `#L12`, `#L12-L34`, `#L12C3-L34C10`.
static LINE_FRAGMENT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^#(?:L\d+(?:C\d+)?-L\d+(?:C\d+)?|L\d+)$").expect("valid regex"));

/// An explicit `{#anchor}` at the end of a heading. markdownlint's own pattern, which admits only
/// lowercase letters and digits.
static ANCHOR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{(#[a-z\d]+(?:[-_][a-z\d]+)*)\}").expect("valid regex"));

/// `\sid\s*=\s*['"]?([^'"\s>]*)`, case-insensitive — markdownlint's `getHtmlAttributeRe`.
static ID_ATTRIBUTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)\sid\s*=\s*['"]?([^'"\s>]*)"#).expect("valid regex"));

static NAME_ATTRIBUTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)\sname\s*=\s*['"]?([^'"\s>]*)"#).expect("valid regex"));

/// Everything GitHub drops from a heading when it makes an anchor: markdownlint's
/// `/[^\p{Letter}\p{Mark}\p{Number}\p{Connector_Punctuation}\- ]/gu`.
static NOT_ANCHOR_TEXT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^\p{L}\p{M}\p{N}\p{Pc}\- ]").expect("valid regex"));

/// A character reference contributes nothing to a heading's anchor — micromark tokenizes `&amp;`
/// as a `characterReference`, which markdownlint's `tokensInclude` set does not list.
static CHARACTER_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"&(?:[A-Za-z][A-Za-z0-9]{1,31}|#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6});")
        .expect("valid regex")
});

/// `[label]: destination`, for the definitions the parser detached.
static REFERENCE_DEFINITION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[[^\]]*\]:[ \t]*(<[^>\n]*>|[^ \t\n]*)").expect("valid regex"));

/// A destination to check once every heading in the document has been seen.
struct Pending {
    range: crate::ast::NodeRange,
    /// The destination, `#` included.
    url: String,
    /// The link's own source, for the message.
    text: String,
}

pub(crate) struct MD051Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    /// The fragments that resolve, with how many headings produced each. This is markdownlint's
    /// `Map`: the count is what makes the second `## Dup` answer to `#dup-1`.
    counts: HashMap<String, usize>,
    /// The same keys in insertion order, because the case-mismatch lookup scans them that way.
    order: Vec<String>,
    pending: Vec<Pending>,
}

impl MD051Linter {
    pub fn new(context: Rc<Context>) -> Self {
        let mut linter = Self {
            context,
            violations: Vec::new(),
            counts: HashMap::new(),
            order: Vec::new(),
            pending: Vec::new(),
        };
        // GitHub resolves `#top` to the top of the page in every document.
        linter.add_fragment("#top".to_string(), 0);
        linter
    }

    fn add_fragment(&mut self, fragment: String, count: usize) {
        if !self.counts.contains_key(&fragment) {
            self.order.push(fragment.clone());
        }
        self.counts.insert(fragment, count);
    }

    /// Records a heading, and the anchors it carries.
    fn add_heading(&mut self, heading: Node) {
        let (raw, text) = {
            let source = self.context.document_content.borrow();
            let raw = match heading_inline(heading) {
                Some(inline) => source[inline.start_byte()..inline.end_byte()].to_string(),
                None => String::new(),
            };
            let mut text = String::new();
            if let Some(inline) = heading_inline(heading) {
                collect_anchor_text(inline, &source, &mut text);
            }
            (raw, text)
        };

        let fragment = github_fragment(&text);
        // A heading that is nothing but markup — `# <https://x.com>` — has no anchor at all, and
        // markdownlint skips its `{#…}` too.
        if fragment == "#" {
            return;
        }
        let count = self.counts.get(&fragment).copied().unwrap_or(0);
        if count > 0 {
            self.add_fragment(format!("{fragment}-{count}"), 0);
        }
        self.add_fragment(fragment, count + 1);

        for capture in ANCHOR.captures_iter(&raw) {
            let anchor = capture[1].to_string();
            if !self.counts.contains_key(&anchor) {
                self.add_fragment(anchor, 1);
            }
        }
    }

    /// Records the `id` of any tag, and the `name` of an `<a>`, as fragments.
    fn add_html_anchors(&mut self, text: &str) {
        let mut index = 0;
        while let Some(offset) = text[index..].find('<') {
            let start = index + offset;
            let Some(end) = text[start..].find('>') else {
                return;
            };
            let tag = &text[start..start + end + 1];
            index = start + end + 1;
            // A comment, a declaration, a CDATA section or a processing instruction is not a tag.
            if tag.starts_with("<!") || tag.starts_with("<?") {
                continue;
            }
            let name = tag[1..]
                .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .next()
                .unwrap_or("")
                .trim_start_matches('/');
            // Only an opening tag defines an anchor.
            if tag.starts_with("</") || name.is_empty() {
                continue;
            }
            let anchor = ID_ATTRIBUTE
                .captures(tag)
                .or_else(|| {
                    (name.eq_ignore_ascii_case("a")).then(|| NAME_ATTRIBUTE.captures(tag))?
                })
                .map(|capture| capture[1].to_string());
            if let Some(anchor) = anchor.filter(|anchor| !anchor.is_empty()) {
                self.add_fragment(format!("#{anchor}"), 0);
            }
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch, collecting the destinations to check and the anchors it passes.
    fn collect_inline(&mut self, root: Node) {
        let mut found = Vec::new();
        let mut anchors = Vec::new();
        let mut stack = vec![root];
        {
            let source = self.context.document_content.borrow();
            while let Some(node) = stack.pop() {
                match node.kind() {
                    "link" => {
                        let range = node.range();
                        let text = source[range.start_byte..range.end_byte].to_string();
                        // Only an inline link carries its own destination; a reference link resolves
                        // to a definition, which is checked where it is written. Told apart by the
                        // link's last byte, since comrak does not expose which form it parsed.
                        if let Some(target) = node.link_target() {
                            if text.ends_with(')') {
                                found.push(Pending {
                                    range,
                                    url: target.url.clone(),
                                    text,
                                });
                            }
                        }
                    }
                    "html_inline" => {
                        anchors.push(source[node.start_byte()..node.end_byte()].to_string());
                    }
                    _ => {}
                }
                for index in 0..node.child_count() {
                    if let Some(child) = node.child(index) {
                        stack.push(child);
                    }
                }
            }
        }

        for anchor in anchors {
            self.add_html_anchors(&anchor);
        }
        found.reverse();
        self.pending.extend(found);
    }

    /// Records a detached definition's destination, which is where markdownlint reports a
    /// `[ref]: #` that resolves to nothing.
    fn collect_definition(&mut self, node: Node) {
        let range = node.range();
        let text = {
            let source = self.context.document_content.borrow();
            source[range.start_byte..range.end_byte].to_string()
        };
        let Some(capture) = REFERENCE_DEFINITION.captures(&text) else {
            return;
        };
        let destination = capture[1].trim_matches(|c| c == '<' || c == '>');
        self.pending.push(Pending {
            range,
            url: destination.to_string(),
            text,
        });
    }

    fn check(&mut self, pending: &Pending) {
        let url = pending.url.as_str();
        if url.len() <= 1 || !url.starts_with('#') {
            return;
        }
        let tail = &url[1..];
        let encoded = format!("#{}", encode_uri_component(tail));
        if self.counts.contains_key(&encoded) || LINE_FRAGMENT.is_match(&encoded) {
            return;
        }
        let settings = &self.context.config.linters.settings.link_fragments;
        if !settings.ignored_pattern.is_empty()
            && Regex::new(&settings.ignored_pattern).is_ok_and(|pattern| pattern.is_match(tail))
        {
            return;
        }

        // A fragment that differs only in case gets markdownlint's "Expected/Actual" detail, and is
        // let through when `ignore_case` is set. Percent-encoded fragments land here too: encoding a
        // heading produces uppercase hex, and a link written with the same escapes matches only once
        // both sides are lowercased.
        let lower = url.to_lowercase();
        let detail = match self.order.iter().find(|key| key.to_lowercase() == lower) {
            Some(key) if key != url && !settings.ignore_case => {
                format!(" Expected: {key}; Actual: {url}")
            }
            Some(_) => return,
            None => String::new(),
        };
        self.violations.push(RuleViolation::new(
            &MD051,
            format!(
                "{}{detail} [Context: \"{}\"]",
                MD051.description, pending.text
            ),
            self.context.file_path.clone(),
            range_from_node_range(&pending.range),
        ));
    }
}

/// The `inline` node holding a heading's text: directly under an ATX heading, under the paragraph
/// of a setext one.
fn heading_inline<'a>(heading: Node<'a>) -> Option<Node<'a>> {
    fn find<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        (0..node.child_count())
            .filter_map(|index| node.child(index))
            .find(|child| child.kind() == kind)
    }
    if heading.kind() == "setext_heading" {
        return find(heading, "paragraph").and_then(|paragraph| find(paragraph, "inline"));
    }
    find(heading, "inline")
}

/// Appends the characters a heading's anchor is built from.
///
/// markdownlint takes micromark's `data`, `codeTextData`, `mathTextData` and `characterEscapeValue`
/// tokens and nothing else, skipping the descendants of an `image`, a `reference` or a `resource`.
/// In this tree that is: the text of every node but an image, a code span's content without its
/// backticks, inline math, and nothing at all for an `html_inline` — with a link's *label* kept,
/// since only its destination is a `resource`.
fn collect_anchor_text(node: Node, source: &str, out: &mut String) {
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        let text = &source[child.start_byte()..child.end_byte()];
        match child.kind() {
            "text" => out.push_str(&clean_text(text)),
            "code_span" => out.push_str(code_content(text)),
            "math" => out.push_str(text),
            "image" | "html_inline" => {}
            _ => collect_anchor_text(child, source, out),
        }
    }
}

/// A text node's characters as micromark would report them: no character references, and no
/// backslash on an escape.
fn clean_text(text: &str) -> String {
    let stripped = CHARACTER_REFERENCE.replace_all(text, "");
    let mut out = String::with_capacity(stripped.len());
    let mut escaped = false;
    for character in stripped.chars() {
        if escaped {
            out.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            out.push(character);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// A code span's content, without the backtick runs that delimit it.
fn code_content(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut open = 0;
    while open < bytes.len() && bytes[open] == b'`' {
        open += 1;
    }
    let mut close = bytes.len();
    while close > open && bytes[close - 1] == b'`' {
        close -= 1;
    }
    &text[open..close]
}

/// GitHub's heading-to-anchor conversion, as markdownlint implements it: lowercase, drop everything
/// that is not a letter, a mark, a number, a connector punctuation character, a hyphen or a space,
/// turn spaces into hyphens, then percent-encode.
fn github_fragment(text: &str) -> String {
    let lowered = text.to_lowercase();
    let filtered = NOT_ANCHOR_TEXT.replace_all(&lowered, "");
    let hyphenated = filtered.replace(' ', "-");
    format!("#{}", encode_uri_component(&hyphenated))
}

/// JavaScript's `encodeURIComponent`: everything outside `A-Za-z0-9-_.!~*'()` becomes the
/// percent-encoded form of its UTF-8 bytes.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl RuleLinter for MD051Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "atx_heading" | "setext_heading" => self.add_heading(*node),
            "inline" => self.collect_inline(*node),
            "html_block" => {
                let text = {
                    let source = self.context.document_content.borrow();
                    source[node.start_byte()..node.end_byte()].to_string()
                };
                self.add_html_anchors(&text);
            }
            "link_reference_definition" => self.collect_definition(*node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        // Taken wholesale because `check` reads the fragments this linter owns while it walks them.
        for pending in std::mem::take(&mut self.pending) {
            self.check(&pending);
        }
        std::mem::take(&mut self.violations)
    }
}

pub const MD051: Rule = Rule {
    id: "MD051",
    alias: "link-fragments",
    tags: &["links"],
    description: "Link fragments should be valid",
    rule_type: RuleType::Document,
    required_nodes: &[
        "atx_heading",
        "setext_heading",
        "inline",
        "html_block",
        "link_reference_definition",
    ],
    new_linter: |context| Box::new(MD051Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD051LinkFragmentsTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    /// `(line, column)` of one invalid fragment, both 1-based, which is markdownlint's `errorRange`.
    type Position = (usize, usize);

    fn config_with(table: MD051LinkFragmentsTable) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("link-fragments", RuleSeverity::Error)],
            LintersSettingsTable {
                link_fragments: table,
                ..Default::default()
            },
        )
    }

    fn test_config() -> crate::config::QuickmarkConfig {
        config_with(MD051LinkFragmentsTable::default())
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
        positions_with(test_config(), source)
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output, run with
    /// only `link-fragments` enabled: its line and its `errorRange` column.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so every link below sits after ASCII
    /// text; the one case that does not is asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &str, &[Position])] = &[
    ("top", "[up](#top)\n", &[]),
    ("missing", "[x](#nope)\n", &[(1, 1)]),
    ("empty_fragment", "[x](#)\n", &[]),
    ("no_fragment", "[x](http://y)\n", &[]),
    ("relative", "[x](a.md#nope)\n", &[]),
    ("line_fragment", "[x](#L12) and [y](#L1-L2) and [z](#L1C2-L3C4)\n", &[]),
    ("bad_line_fragment", "[x](#L12X)\n", &[(1, 1)]),
    ("dup_headings", "# Dup\n\n# Dup\n\n# Dup\n\n[a](#dup) [b](#dup-1) [c](#dup-2) [d](#dup-3)\n", &[(7, 35)]),
    ("anchor", "# Heading {#custom-anchor}\n\n[a](#custom-anchor) [b](#heading-custom-anchor)\n", &[]),
    ("html_anchor_block", "<a name=\"Screenshot\"></a>\n\n[a](#Screenshot)\n", &[]),
    ("html_anchor_inline", "text <a id=\"x\"></a> more\n\n[a](#x)\n", &[]),
    ("html_anchor_div", "<div id=\"y\">z</div>\n\n[a](#y)\n", &[]),
    ("html_anchor_close", "</a name=\"z\">\n\n[a](#z)\n", &[(3, 1)]),
    ("html_anchor_comment", "<!-- <a id=\"q\"> -->\n\n[a](#q)\n", &[(3, 1)]),
    ("ref_definition_hash", "[a][r]\n\n[r]: #\n", &[]),
    ("ref_definition_url", "[a][r]\n\n[r]: http://x\n", &[]),
    ("ref_definition_missing", "[a][r]\n\n[r]: #nope\n", &[(3, 1)]),
    ("case_mismatch", "# Heading\n\n[a](#heading) [b](#Heading) [c](#HEADING)\n", &[(3, 15), (3, 29)]),
    ("in_heading", "# [a](#nope) title\n", &[(1, 3)]),
    ("in_table", "| a |\n|---|\n| [x](#nope) |\n", &[(3, 3)]),
    ("in_blockquote", "> [x](#nope)\n", &[(1, 3)]),
    ("in_list", "- [x](#nope)\n", &[(1, 3)]),
    ("in_code_span", "`[x](#nope)`\n", &[]),
    ("in_fenced_code", "```\n[x](#nope)\n```\n", &[]),
    ("image_fragment", "![x](#nope)\n", &[]),
    ("two_on_a_line", "[a](#nope) and [b](#alsonope)\n", &[(1, 1), (1, 16)]),
    ("nested_link", "[outer [inner](#nope) text](#alsonope)\n", &[(1, 8)]),
    ("angle_destination", "[x](<#nope>)\n", &[(1, 1)]),
    ("with_title", "[x](#nope \"title\")\n", &[(1, 1)]),
    ("position_second_line", "text\n\n# H\n\n[x](#nope)\n", &[(5, 1)]),
    ("shortcut_ref", "[a]\n\n[a]: #nope\n", &[(3, 1)]),
    ("front_matter", "---\ntitle: x\n---\n\n# H\n\n[a](#h) [b](#nope)\n", &[(7, 9)]),
    ("emphasis_heading", "# With *emph* and `code`\n\n[a](#with-emph-and-code) [b](#nope)\n", &[(3, 26)]),
    ("link_in_heading", "# With [label](http://x)\n\n[a](#with-label) [b](#nope)\n", &[(3, 18)]),
    ("image_in_heading", "# With ![alt](http://y)\n\n[a](#with-) [b](#nope)\n", &[(3, 13)]),
    ("html_in_heading", "# With <b>html</b>\n\n[a](#with-html) [b](#nope)\n", &[(3, 17)]),
    ("setext_heading", "Some Heading\n==============\n\n[a](#some-heading) [b](#nope)\n", &[(4, 20)]),
    ("no_trailing_newline", "# H\n\n[x](#nope)", &[(3, 1)]),
    ("crlf", "# H\r\n\r\n[x](#nope)\r\n", &[(3, 1)]),
    ("empty_document", "", &[]),
    ("heading_only", "# Just a heading\n", &[]),
    ("autolink_heading", "# <https://x.com>\n\n[a](#) [b](#nope)\n", &[(3, 8)]),
    ("entity_heading", "# With &amp; entity\n\n[a](#with--entity)\n", &[]),
    ("escape_heading", "# With an escape \\* here\n\n[a](#with-an-escape--here)\n", &[]),
    ("math_heading", "# a $x^2$ math\n\n[a](#a-x2-math)\n", &[]),
    ("tab_heading", "#\ttab separated\n\n[a](#tab-separated)\n", &[]),
    ("closed_atx", "# Empty-ish ###\n\n[a](#empty-ish)\n", &[]),
    ("empty_atx", "###\n\n[a](#nope)\n", &[(3, 1)]),
    ("multiline_setext", "Multi line setext\nsecond line\n==================\n\n[a](#multi-line-setextsecond-line)\n", &[]),
    ];

    /// Headings and the fragment markdownlint resolves each one to. The fragments come from
    /// markdownlint's own `convertHeadingToHTMLFragment` run over micromark's own tokens, and every
    /// one was then checked end to end: markdownlint reports nothing for a link to it and one
    /// violation for a link to a fragment no heading produces.
    const HEADINGS: &[(&str, &str)] = &[
        ("# Simple", "#simple"),
        ("## Two words", "#two-words"),
        ("# Trailing spaces   ", "#trailing-spaces"),
        ("#  Leading spaces", "#leading-spaces"),
        ("# With `code`", "#with-code"),
        (
            "# With *emphasis* and **strong**",
            "#with-emphasis-and-strong",
        ),
        ("# With [a link](http://x)", "#with-a-link"),
        ("# With ![an image](http://y)", "#with-"),
        ("# With <b>html</b>", "#with-html"),
        ("# With an escape \\* here", "#with-an-escape--here"),
        ("# With &amp; entity", "#with--entity"),
        (
            "# Punctuation: commas, dots. And dashes - and _underscores_",
            "#punctuation-commas-dots-and-dashes---and-underscores",
        ),
        ("# 你好世界", "#%E4%BD%A0%E5%A5%BD%E4%B8%96%E7%95%8C"),
        (
            "# Mixed 你好 and English",
            "#mixed-%E4%BD%A0%E5%A5%BD-and-english",
        ),
        ("# Emoji 🎉 party", "#emoji--party"),
        ("# Already-hyphenated", "#already-hyphenated"),
        ("# Multiple   spaces", "#multiple---spaces"),
        ("#\ttab separated", "#tab-separated"),
        ("# CAPITAL LETTERS", "#capital-letters"),
        ("# Digits 123 and 456", "#digits-123-and-456"),
        ("# `code with spaces`", "#code-with-spaces"),
        ("# a $x^2$ math", "#a-x2-math"),
        ("# trailing hyphen -", "#trailing-hyphen--"),
        ("# - leading hyphen", "#--leading-hyphen"),
        ("# <a id=\"anchor\"></a> Heading", "#-heading"),
        ("# Heading {#custom-anchor}", "#heading-custom-anchor"),
        ("Setext one\n==========", "#setext-one"),
        ("Setext two\n==========", "#setext-two"),
        (
            "Setext with `code`\n==================",
            "#setext-with-code",
        ),
        (
            "Multi line setext\nsecond line\n===========",
            "#multi-line-setextsecond-line",
        ),
        ("# [ref][] heading", "#ref-heading"),
        ("# Empty-ish ###", "#empty-ish"),
        ("# `a` and `b`", "#a-and-b"),
        ("# a/b/c", "#abc"),
        ("# 100% done", "#100-done"),
        ("# C++ templates", "#c-templates"),
        ("# foo_bar_baz", "#foo_bar_baz"),
        ("# ~~strike~~", "#strike"),
        ("# <https://x.com>", "#"),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(name, source, expected) in CASES {
            assert_eq!(expected, positions(source).as_slice(), "case `{name}`");
        }
    }

    #[test]
    fn heading_fragments_resolve_both_ways() {
        for &(heading, fragment) in HEADINGS {
            let resolved = format!("{heading}\n\n[link]({fragment})\n");
            assert_eq!(
                0,
                positions(&resolved).len(),
                "heading {heading:?} should answer to {fragment}"
            );
            let unresolved = format!("{heading}\n\n[link](#definitely-not-a-real-anchor)\n");
            assert_eq!(
                1,
                positions(&unresolved).len(),
                "heading {heading:?} should not answer to a fragment of its own"
            );
        }
    }

    /// A link after a multi-byte character. markdownlint counts UTF-16 units, so its column is
    /// smaller than the byte-based one quickmark reports; only the count agrees. That is the
    /// byte-column convention every rule shares, not an MD051 difference.
    #[test]
    fn positions_count_bytes() {
        // markdownlint: [(3, 10)] for the `[b](#nope)` after `[a](#你好) `
        let source = "# 你好\n\n[a](#你好) [b](#nope)\n";
        assert_eq!(vec![(3, 14)], positions(source));
    }

    #[test]
    fn ignore_case_accepts_a_differently_cased_fragment() {
        let source = "# Heading\n\n[a](#heading) [b](#Heading) [c](#HEADING) [d](#nope)\n";
        // markdownlint with `ignore_case: true` reports only the fourth link.
        let config = config_with(MD051LinkFragmentsTable {
            ignore_case: true,
            ..Default::default()
        });
        assert_eq!(vec![(3, 43)], positions_with(config, source));
    }

    #[test]
    fn ignored_pattern_skips_matching_fragments() {
        let source = "# Heading\n\n[a](#heading) [b](#Heading) [c](#HEADING) [d](#nope)\n";
        // markdownlint with `ignored_pattern: "^no"` reports the two case mismatches and not `[d]`.
        let config = config_with(MD051LinkFragmentsTable {
            ignored_pattern: "^no".to_string(),
            ..Default::default()
        });
        assert_eq!(vec![(3, 15), (3, 29)], positions_with(config, source));
    }

    #[test]
    fn a_case_mismatch_says_what_was_expected() {
        let violation = {
            let mut linter = MultiRuleLinter::new_for_document(
                PathBuf::from("test.md"),
                test_config(),
                "# Heading\n\n[a](#HEADING)\n",
            );
            linter.analyze().pop().expect("one violation")
        };
        assert_eq!(
            "Link fragments should be valid Expected: #heading; Actual: #HEADING \
             [Context: \"[a](#HEADING)\"]",
            violation.message()
        );
    }

    #[test]
    fn a_document_without_links_is_quiet() {
        assert_eq!(0, positions("# Heading\n\nplain text\n").len());
    }
}
