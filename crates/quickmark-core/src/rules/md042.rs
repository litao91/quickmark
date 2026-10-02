use std::rc::Rc;

use crate::ast::Node;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

/// MD042 - No empty links
///
/// Reports a link whose destination is empty or a bare `#`. The links come from the tree, so a
/// label with escaped or nested brackets is a link here exactly when it is one in CommonMark —
/// `[\[1\]](#)` and `[[a]](#)` are, and neither survives a `\[([^\]]*)\]` regex.
pub(crate) struct MD042Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD042Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch. It descends into a link's own children too, since `[[a]()](http://x)` puts
    /// an empty link inside a sound one.
    fn feed_inline(&mut self, root: Node) {
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            if node.kind() == "link" && self.is_empty_link(node) {
                let range = node.range();
                self.violations.push(RuleViolation::new(
                    &MD042,
                    MD042.description.to_string(),
                    self.context.file_path.clone(),
                    range_from_node_range(&range),
                ));
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

    fn is_empty_link(&self, node: Node) -> bool {
        let Some(target) = node.link_target() else {
            return false;
        };
        let url = target.url.trim();
        if url == "#" {
            return true;
        }
        if !url.is_empty() {
            return false;
        }
        // An empty destination is only a violation on an inline link with no title. markdownlint
        // reads micromark's token stream, which keeps an empty destination *string* for `[a]( "t")`
        // and therefore reports only `#`; a reference link resolves to its definition, and
        // `[r]: <>` is a destination micromark never produced a string for. comrak hands over the
        // decoded url and title alone, so the two forms are told apart by the link's last source
        // byte: `)` for `[a](…)`, `]` for `[a][r]`, `[a][]` and `[a]`.
        if !target.title.is_empty() {
            return false;
        }
        let content = self.context.document_content.borrow();
        node.utf8_text(content.as_bytes())
            .is_ok_and(|text| text.ends_with(')'))
    }
}

impl RuleLinter for MD042Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "inline" {
            self.feed_inline(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD042: Rule = Rule {
    id: "MD042",
    alias: "no-empty-links",
    tags: &["links"],
    description: "No empty links",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD042Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("no-empty-links", RuleSeverity::Error)])
    }

    /// `(line, column, width)` of one violation, 1-based. The width is the whole link, which is
    /// what markdownlint's `errorRange` covers.
    type Link = (usize, usize, usize);

    fn links(source: &str) -> Vec<Link> {
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
    /// only `no-empty-links` enabled: its line and its `errorRange` column and length.
    ///
    /// Columns count UTF-8 bytes here and UTF-16 units there, so the one case with a multi-byte
    /// character before the link is asserted separately in [`positions_count_bytes`].
    const CASES: &[(&str, &str, &[Link])] = &[
        ("plain", "[a](http://x)\n", &[]),
        ("empty_dest", "[a]()\n", &[(1, 1, 5)]),
        ("hash_dest", "[a](#)\n", &[(1, 1, 6)]),
        ("fragment_dest", "[a](#frag)\n", &[]),
        ("space_dest", "[a]( )\n", &[(1, 1, 6)]),
        ("angle_empty", "[a](<>)\n", &[(1, 1, 7)]),
        ("angle_hash", "[a](<#>)\n", &[(1, 1, 8)]),
        ("angle_hash_space", "[a](< # >)\n", &[(1, 1, 10)]),
        ("empty_with_title", "[a]( \"t\")\n", &[]),
        ("hash_with_title", "[a](# \"t\")\n", &[(1, 1, 10)]),
        ("url_with_title", "[a](http://x \"t\")\n", &[]),
        ("hash_padded", "[a](  #  )\n", &[(1, 1, 10)]),
        ("full_ref_url", "[a][r]\n\n[r]: http://x\n", &[]),
        ("full_ref_hash", "[a][r]\n\n[r]: #\n", &[(1, 1, 6)]),
        ("full_ref_empty", "[a][r]\n\n[r]: <>\n", &[]),
        ("full_ref_undefined", "[a][nope]\n", &[]),
        ("collapsed_ref_url", "[a][]\n\n[a]: http://x\n", &[]),
        ("collapsed_ref_hash", "[a][]\n\n[a]: #\n", &[(1, 1, 5)]),
        ("collapsed_ref_undefined", "[a][]\n", &[]),
        ("shortcut_url", "[a]\n\n[a]: http://x\n", &[]),
        ("shortcut_hash", "[a]\n\n[a]: #\n", &[(1, 1, 3)]),
        ("shortcut_undefined", "[a]\n", &[]),
        ("image_empty", "![a]()\n", &[]),
        ("image_hash", "![a](#)\n", &[]),
        ("autolink_http", "<http://x>\n", &[]),
        ("autolink_mail", "<a@b.c>\n", &[]),
        ("gfm_autolink", "see http://x here\n", &[]),
        ("escaped_brackets", "[\\[1\\]](#)\n", &[(1, 1, 10)]),
        ("nested_brackets", "[[a]](#)\n", &[(1, 1, 8)]),
        ("in_code_span", "`[a]()`\n", &[]),
        ("in_code_block", "```\n[a]()\n```\n", &[]),
        ("in_heading", "# [a]()\n", &[(1, 3, 5)]),
        ("in_table", "| x |\n|---|\n| [a]() |\n", &[(3, 3, 5)]),
        ("in_blockquote", "> [a]()\n", &[(1, 3, 5)]),
        (
            "two_on_a_line",
            "[a]() and [b](#) and [c](http://x)\n",
            &[(1, 1, 5), (1, 11, 6)],
        ),
        ("two_one_empty", "[a](http://x) and [b]()\n", &[(1, 19, 5)]),
        ("empty_label", "[]()\n", &[(1, 1, 4)]),
        ("label_only_brackets", "[[]]()\n", &[(1, 1, 6)]),
        ("in_link_label", "[[a]()](http://x)\n", &[(1, 2, 5)]),
        ("ref_in_label", "[a [b]() c](http://x)\n", &[(1, 4, 5)]),
        ("no_trailing_newline", "[a]()", &[(1, 1, 5)]),
        ("crlf", "[a]()\r\n[b](#)\r\n", &[(1, 1, 5), (2, 1, 6)]),
        (
            "footnote_like",
            "[^gh-md]: <> \"Like here on GitHub.\"\n",
            &[],
        ),
        (
            "front_matter",
            "---\ntitle: [a]()\n---\n\n[b]()\n",
            &[(5, 1, 5)],
        ),
        ("emph_around", "*[a]()*\n", &[(1, 2, 5)]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(name, source, expected) in CASES {
            assert_eq!(expected, links(source).as_slice(), "case `{name}`");
        }
    }

    /// A link after a multi-byte character. markdownlint counts UTF-16 units, so its column is
    /// smaller than the byte-based one quickmark reports; only the count and width agree. That is
    /// the byte-column convention every rule shares, not an MD042 difference.
    #[test]
    fn positions_count_bytes() {
        assert_eq!(vec![(1, 5, 5)], links("你 [a]() 好\n"));
    }

    /// `[a](<> "t")` is a violation in markdownlint and is not here. micromark emits no destination
    /// string for an angle-wrapped empty destination, so markdownlint falls through to its
    /// unconditional branch; comrak reports an empty url and a title, which is indistinguishable
    /// from `[a]( "t")` — the one shape markdownlint deliberately does *not* report. Choosing the
    /// rarer false negative over the rarer false positive.
    #[test]
    fn an_empty_angle_destination_with_a_title_is_missed() {
        assert_eq!(0, links("[a](<> \"t\")\n").len());
        assert_eq!(1, links("[a](<>)\n").len());
    }

    /// A link split across lines. markdownlint throws — its range spans two lines and it validates
    /// that against the line's length — so there is no measured value to match, and the width
    /// quickmark reports is the end column on the *last* line, which spans nothing. Only the fact
    /// that the link is found, on the line it opens on, is asserted. Reporting beats aborting the
    /// rule for the whole file, which is what markdownlint does.
    #[test]
    fn a_link_across_lines_is_still_reported() {
        for source in ["[a\nb]()\n", "[a\nb](#)\n"] {
            let found = links(source);
            assert_eq!(1, found.len(), "source {source:?}");
            assert_eq!((1, 1), (found[0].0, found[0].1), "source {source:?}");
        }
    }
}
