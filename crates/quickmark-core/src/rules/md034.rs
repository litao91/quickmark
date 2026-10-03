use std::rc::Rc;

use crate::ast::Node;
use linkify::{LinkFinder, LinkKind};

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{md037::is_escaped, Context, Rule, RuleLinter, RuleType},
};

/// Inline subtrees to stay out of. micromark tokenizes a link or image label as a *label*, and does
/// not run the autolink-literal extension inside one, so `[see https://x.com here](y)` is quiet —
/// and so is `[see https://x.com here]`, resolved or not. Their brackets still count for
/// [`in_label_attempt`], which is why this stops the walk rather than blanking the span.
const UNSCANNED: &[&str] = &["link", "image"];

/// Leaves whose contents are literal by construction, so a bracket in one opens no label.
const OPAQUE: &[&str] = &["code_span", "math"];

pub(crate) struct MD034Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    finder: LinkFinder,
}

impl MD034Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // linkify only looks for scheme-less domains when told to, and GFM autolinks
        // `www.example.com` without a scheme. Everything else it would find that way —
        // `example.com`, `foo.bar` — is literal text to micromark, so `is_gfm_autolink` filters it
        // back out.
        let mut finder = LinkFinder::new();
        finder.url_must_have_scheme(false);
        Self {
            context,
            violations: Vec::new(),
            finder,
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch, and looks for bare URLs in the `text` nodes it finds.
    ///
    /// Code spans, inline math and HTML are leaves, so their contents are never scanned; link and
    /// image subtrees are skipped whole. What is left is exactly the text micromark would have run
    /// its autolink-literal extension over.
    fn check_inline(&mut self, root: Node) {
        let context = Rc::clone(&self.context);
        let source = context.document_content.borrow();

        let mut text = Vec::new();
        let mut opaque = Vec::new();
        let mut tags = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let kind = node.kind();
            if UNSCANNED.contains(&kind) {
                continue;
            }
            if kind == "text" {
                text.push((node.start_byte(), node.end_byte()));
            } else if OPAQUE.contains(&kind) {
                opaque.push((node.start_byte(), node.end_byte()));
            } else if kind == "html_inline" {
                tags.push(node);
            }
            for index in 0..node.child_count() {
                if let Some(child) = node.child(index) {
                    stack.push(child);
                }
            }
        }
        text.sort_unstable();
        // The walk pops siblings in reverse, and tag pairing reads left to right.
        tags.sort_unstable_by_key(|tag| tag.start_byte());
        opaque.extend(html_tag_pairs(&tags, &source));

        let mut found = Vec::new();
        for (start, end) in text {
            for link in self.finder.links(&source[start..end]) {
                let url = start + link.start();
                let url_end = start + link.end();
                if is_bare(
                    &source,
                    root.start_byte(),
                    url,
                    url_end,
                    link.as_str(),
                    link.kind(),
                    &opaque,
                ) {
                    found.push((url, url_end, link.as_str().to_string()));
                }
            }
        }

        drop(source);
        for (start, end, url) in found {
            self.push(start, end, &url);
        }
    }

    fn push(&mut self, start: usize, end: usize, url: &str) {
        let range = crate::ast::NodeRange {
            start_byte: start,
            end_byte: end,
            start_point: self.context.point_at(start),
            end_point: self.context.point_at(end),
        };
        self.violations.push(RuleViolation::new(
            &MD034,
            format!("{} [Context: \"{}\"]", MD034.description, url),
            self.context.file_path.clone(),
            range_from_node_range(&range),
        ));
    }
}

/// Whether `url` starts with one of the schemes GFM's autolink-literal extension recognises. Those,
/// plus email addresses, are the only things micromark turns into a `literalAutolink` token, so
/// `ftp://`, `file://`, `git://` and `oss://` are literal text and MD034 must leave them alone.
pub(crate) fn is_gfm_autolink(url: &str) -> bool {
    ["http://", "https://", "www."].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

/// The spans between a matching pair of inline HTML tags.
///
/// markdownlint drops everything a token holds between an open tag and its close before looking for
/// autolink literals, so `a <b>https://x.com</b> c` is quiet. Tags are matched by name with
/// nesting, exactly as its `getHtmlTagInfo` does; an open tag with no close suppresses nothing.
fn html_tag_pairs(tags: &[Node], source: &str) -> Vec<(usize, usize)> {
    let info = |node: &Node| {
        node.utf8_text(source.as_bytes())
            .ok()
            .and_then(html_tag_info)
    };
    let mut pairs = Vec::new();
    let mut index = 0;
    while index < tags.len() {
        let Some((name, false)) = info(&tags[index]) else {
            index += 1;
            continue;
        };
        let mut depth = 1;
        let mut close = None;
        for (offset, tag) in tags[index + 1..].iter().enumerate() {
            let Some((candidate_name, is_close)) = info(tag) else {
                continue;
            };
            if candidate_name != name {
                continue;
            }
            if is_close {
                depth -= 1;
                if depth == 0 {
                    close = Some((index + 1 + offset, *tag));
                    break;
                }
            } else {
                depth += 1;
            }
        }
        match close {
            Some((position, tag)) => {
                pairs.push((tags[index].start_byte(), tag.end_byte()));
                index = position + 1;
            }
            None => index += 1,
        }
    }
    pairs
}

/// `(name, is_close)` for an inline HTML tag, or `None` for a comment, a CDATA section, a
/// processing instruction or a declaration. markdownlint's own test is `/^<([^!>][^/\s>]*)/`,
/// which takes a leading `/` as part of the name and reads it as the close.
fn html_tag_info(tag: &str) -> Option<(&str, bool)> {
    let rest = tag.strip_prefix('<')?;
    let first = rest.as_bytes().first()?;
    if *first == b'!' || *first == b'>' {
        return None;
    }
    let name_end = rest[1..]
        .find(|c: char| c == '/' || c.is_whitespace() || c == '>')
        .map_or(rest.len(), |offset| offset + 1);
    let name = &rest[..name_end];
    let close = name.starts_with('/');
    Some((name.strip_prefix('/').unwrap_or(name), close))
}

fn is_bare(
    source: &str,
    inline_start: usize,
    start: usize,
    end: usize,
    url: &str,
    kind: &LinkKind,
    opaque: &[(usize, usize)],
) -> bool {
    if opaque.iter().any(|&(from, to)| from <= start && start < to) {
        return false;
    }
    let bytes = source.as_bytes();
    let autolink = match kind {
        LinkKind::Url => {
            if !is_gfm_autolink(url) {
                return false;
            }
            // micromark/micromark#164: `<www.x.com>` tokenizes as data, a literalAutolink and data
            // again, and markdownlint skips that triple. A scheme'd `<https://x.com>` is a real
            // autolink node in comrak, so it never reaches here.
            let bracketed = start.checked_sub(1).and_then(|before| bytes.get(before))
                == Some(&b'<')
                && bytes.get(end) == Some(&b'>');
            !bracketed && can_start_here(bytes, start, url, kind)
        }
        LinkKind::Email => can_start_here(bytes, start, url, kind),
        _ => false,
    };
    autolink && !in_label_attempt(source, inline_start, start, opaque)
}

/// GFM's own test on the character before an autolink literal, which differs per form: a protocol
/// URL may follow anything but an ASCII letter (`5https://x.com` is bare, `ahttps://x.com` is
/// text), `www.` may follow only a short list of characters, and an email may follow neither a
/// slash nor an `atext` character.
fn can_start_here(bytes: &[u8], start: usize, url: &str, kind: &LinkKind) -> bool {
    let Some(&previous) = start.checked_sub(1).and_then(|before| bytes.get(before)) else {
        return true;
    };
    let scheme_less = url
        .get(..4)
        .is_some_and(|head| head.eq_ignore_ascii_case("www."));
    if scheme_less {
        return matches!(previous, b'(' | b'*' | b'_' | b'[' | b']' | b'~')
            || previous.is_ascii_whitespace();
    }
    match kind {
        LinkKind::Email => {
            !matches!(previous, b'/' | b'+' | b'-' | b'.' | b'_')
                && !previous.is_ascii_alphanumeric()
        }
        _ => !previous.is_ascii_alphabetic(),
    }
}

/// Whether an unclosed `[` precedes the URL.
///
/// micromark spends everything from a `[` to its matching `]` on a label attempt and does not look
/// for autolink literals inside one, whether or not the label ever resolves: `[a https://x.com b]`
/// is quiet and `[a] https://x.com` is not. Brackets in a code span or in inline math are not
/// brackets as far as that is concerned, and an escaped one is text.
fn in_label_attempt(
    source: &str,
    inline_start: usize,
    start: usize,
    opaque: &[(usize, usize)],
) -> bool {
    let bytes = source.as_bytes();
    let mut closed = 0usize;
    let mut index = start;
    while index > inline_start {
        index -= 1;
        let byte = bytes[index];
        if byte != b'[' && byte != b']' {
            continue;
        }
        if opaque.iter().any(|&(from, to)| from <= index && index < to) {
            continue;
        }
        if is_escaped(source, index) {
            continue;
        }
        if byte == b']' {
            closed += 1;
        } else if closed == 0 {
            return true;
        } else {
            closed -= 1;
        }
    }
    false
}

impl RuleLinter for MD034Linter {
    fn feed(&mut self, node: &Node) {
        // `inline`, not `paragraph`: micromark's `literalAutolink` tokens live in headings and table
        // cells too, and matching only paragraphs left every bare URL in a table unreported.
        if node.kind() == "inline" {
            self.check_inline(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD034: Rule = Rule {
    id: "MD034",
    aliases: &["no-bare-urls"],
    tags: &["links", "url"],
    description: "Bare URL used",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD034Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    /// `(line, column, width)` of one bare URL, 1-based. The width is the URL's own length, which
    /// is what markdownlint's `errorRange` covers.
    type Url = (usize, usize, usize);

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-bare-urls", RuleSeverity::Error)])
    }

    fn urls(source: &str) -> Vec<Url> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), source);
        linter
            .analyze()
            .iter()
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line + 1,
                    range.start.character + 1,
                    range.end.character - range.start.character,
                )
            })
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output, run with
    /// only `no-bare-urls` enabled: its line and its `errorRange` column and length.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so every source below keeps the
    /// characters before a URL ASCII.
    const CASES: &[(&str, &[Url])] = &[
        ("see https://example.com here\n", &[(1, 5, 19)]),
        ("see www.example.com here\n", &[(1, 5, 15)]),
        ("see example.com and foo.bar here\n", &[]),
        ("see ftp://example.com and file:///x here\n", &[]),
        ("HTTPS://EXAMPLE.COM\n", &[(1, 1, 19)]),
        ("mail me at a@b.com please\n", &[(1, 12, 7)]),
        ("see <https://example.com> here\n", &[]),
        ("<www.example.com>\n", &[]),
        ("<a@b.com>\n", &[]),
        ("see <https://example.com x> here\n", &[(1, 6, 19)]),
        ("see `https://example.com` here\n", &[]),
        ("$https://x.com$\n", &[]),
        ("[t](https://example.com)\n", &[]),
        ("![t](https://example.com)\n", &[]),
        ("[see https://example.com here](http://y)\n", &[]),
        ("*see https://example.com*\n", &[(1, 6, 19)]),
        ("a <b>https://example.com</b> c\n", &[]),
        ("text <img src='https://example.com'> more\n", &[]),
        ("a <b>https://example.com c\n", &[(1, 6, 19)]),
        ("# see https://example.com\n", &[(1, 7, 19)]),
        ("| a |\n|---|\n| https://example.com |\n", &[(3, 3, 19)]),
        ("> https://example.com\n", &[(1, 3, 19)]),
        ("- https://example.com\n", &[(1, 3, 19)]),
        ("- [x] https://example.com\n", &[(1, 7, 19)]),
        ("```\nhttps://example.com\n```\n", &[]),
        ("text\n\n    https://example.com\n", &[]),
        ("see https://x.com. done\n", &[(1, 5, 13)]),
        ("see https://x.com, done\n", &[(1, 5, 13)]),
        ("see https://x.com/ done\n", &[(1, 5, 14)]),
        ("see (https://x.com/(y)) done\n", &[(1, 6, 17)]),
        ("see https://x.com) done\n", &[(1, 5, 13)]),
        ("see https://x.com?q=1. done\n", &[(1, 5, 17)]),
        ("a@b.com. and (a@b.com)\n", &[(1, 1, 7), (1, 15, 7)]),
        ("www.x.com. and www.x.com,\n", &[(1, 1, 9), (1, 16, 9)]),
        (
            "https://a.com and https://b.com\n",
            &[(1, 1, 13), (1, 19, 13)],
        ),
        (
            "see https://x.org/a\nand https://x.org/b\n",
            &[(1, 5, 15), (2, 5, 15)],
        ),
        ("ahttps://example.com\n", &[]),
        ("2025-05-15https://x.com/a\n", &[(1, 11, 15)]),
        ("x/https://example.com\n", &[(1, 3, 19)]),
        (
            "-https://x.com _https://x.com (https://x.com)\n",
            &[(1, 2, 13), (1, 17, 13), (1, 32, 13)],
        ),
        ("x-www.example.com\n", &[]),
        ("x/www.example.com\n", &[]),
        ("(*_[~] www.example.com\n", &[(1, 8, 15)]),
        ("[a https://x.com b]\n", &[]),
        ("[a https://x.com b\n", &[]),
        ("[a] https://x.com\n", &[(1, 5, 13)]),
        ("[a](b) https://x.com\n", &[(1, 8, 13)]),
        ("[a [b] https://x.com\n", &[]),
        ("a [b https://x.com c] d https://y.com e\n", &[(1, 25, 13)]),
        ("`[` https://x.com\n", &[(1, 5, 13)]),
        ("\\[https://x.com]\n", &[(1, 3, 13)]),
        ("[\nhttps://x.com\n]\n", &[]),
        ("![a https://x.com b](y)\n", &[]),
        (
            "[\n\ncache\n\n](https://www.zhihu.com/topic/20001795)\n",
            &[(5, 3, 36)],
        ),
        ("see https://x.com", &[(1, 5, 13)]),
        (
            "see https://x.com\r\nand https://y.com\r\n",
            &[(1, 5, 13), (2, 5, 13)],
        ),
        ("see https://x.org/a  \nmore\n", &[(1, 5, 15)]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, urls(source).as_slice(), "source {source:?}");
        }
    }

    /// Three measured gaps, each a difference between linkify and micromark rather than a mistake
    /// in the walk:
    ///
    /// - `5https://x.com` is bare to markdownlint at 1:2 and is missed here, because linkify does
    ///   not report a URL glued to a single preceding digit. Two digits (`15https://x.com`) and a
    ///   date (`2025-05-15https://x.com/a`) both work.
    /// - `oss://user:pass@host/path` holds an email micromark autolinks at the `:` and linkify
    ///   swallows into the surrounding `oss://` URL, which is not a GFM scheme and so is dropped.
    /// - `$` delimits inline math by different rules in comrak and micromark, so a URL that one
    ///   parser puts inside a math span the other leaves outside. comrak refuses a closing `$`
    ///   preceded by a space and micromark accepts it.
    /// - GFM lets an autolink path run over `[` and over `\`, so `https://x.com[a]` is one
    ///   15-column URL to markdownlint and `https://x.org/a;\` a 17-column one; linkify stops at
    ///   the bracket and at the backslash. Same start, so only the underline is shorter.
    #[test]
    fn known_differences_from_markdownlint() {
        // markdownlint: [(1, 2, 13)]
        assert_eq!(0, urls("5https://x.com\n").len());
        // markdownlint: [(1, 22, 40)] for the email inside the `oss://` URL
        assert_eq!(
            0,
            urls("oss://key:secret@oss-cn-shanghai.aliyuncs.com/bucket\n").len()
        );
        // markdownlint: [(1, 1, 15)]
        assert_eq!(vec![(1, 1, 13)], urls("https://x.com[a]\n"));
        // markdownlint: [(1, 5, 17)]
        assert_eq!(vec![(1, 5, 15)], urls("see https://x.org/a;\\\nmore\n"));
        // micromark's math text closes on a `$` preceded by whitespace, where comrak refuses one,
        // so the URL below is inside a math span for markdownlint and bare text here.
        // markdownlint: []
        assert_eq!(
            vec![(1, 15, 13)],
            urls("latest=$(curl https://x.com);\\\ncd $y\n")
        );
        // markdownlint: []
        assert_eq!(vec![(1, 10, 13)], urls("a $b and https://x.com $c d\n"));
    }

    #[test]
    fn a_document_without_urls_is_quiet() {
        assert_eq!(0, urls("plain text with no links at all\n").len());
    }
}
