use std::collections::HashMap;
use std::rc::Rc;

use crate::config::RuleSeverity;
use crate::linter::{Context, RuleLinter};

pub mod md001;
pub mod md003;
pub mod md004;
pub mod md005;
pub mod md007;
pub mod md009;
pub mod md010;
pub mod md011;
pub mod md012;
pub mod md013;
pub mod md014;
pub mod md018;
pub mod md019;
pub mod md020;
pub mod md021;
pub mod md022;
pub mod md023;
pub mod md024;
pub mod md025;
pub mod md026;
pub mod md027;
pub mod md028;
pub mod md029;
pub mod md030;
pub mod md031;
pub mod md032;
pub mod md033;
pub mod md034;
pub mod md035;
pub mod md036;
pub mod md037;
pub mod md038;
pub mod md039;
pub mod md040;
pub mod md041;
pub mod md042;
pub mod md043;
pub mod md044;
pub mod md045;
pub mod md046;
pub mod md047;
pub mod md048;
pub mod md049;
pub mod md050;
pub mod md051;
pub mod md052;
pub mod md053;
pub mod md054;
pub mod md055;
pub mod md056;
pub mod md058;
pub mod md059;
pub mod md060;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleType {
    /// Rules that primarily analyze raw text lines (e.g., line length, whitespace)
    Line,
    /// Rules that analyze specific AST node types (e.g., headings, lists, code blocks)
    Token,
    /// Rules that require full document analysis (e.g., duplicate headings, cross-references)
    Document,
    /// Rules that need both AST nodes and line context (blank line spacing around elements)
    Hybrid,
}

/// Whether a line is blank, in markdownlint's sense (`helpers.cjs:isBlankLine`). MD022, MD031,
/// MD032, MD047 and MD058 all ask this same question, and the answer is not "holds no
/// non-whitespace": a line of nothing but block quote markers separates two quotes without being
/// content, and an HTML comment is invisible.
pub(crate) fn is_blank_line(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }
    without_comments(line).replace('>', "").trim().is_empty()
}

/// The line with every complete HTML comment cut out, mirroring markdownlint's `removeComments`. An
/// unterminated `<!--` swallows the rest of the line and an unmatched `-->` the part before it,
/// both of which leave whatever is outside.
fn without_comments(line: &str) -> String {
    let mut out = String::from(line);
    loop {
        let start = out.find("<!--");
        let end = out.find("-->");
        if let Some(end) = end.filter(|&end| start.is_none_or(|start| end < start)) {
            out.drain(..end + 3);
        } else if let (Some(start), Some(end)) = (start, end) {
            out.replace_range(start..end + 3, "");
        } else if let Some(start) = start {
            out.truncate(start);
            return out;
        } else {
            return out;
        }
    }
}

/// Prepares the context a message quotes, exactly as markdownlint does: `addErrorContext` normalizes
/// line endings and calls `helpers.cjs:ellipsify`, which over thirty characters keeps the start, the
/// end, or fifteen of each — `start` and `end` say which the rule cares about, and a rule that cares
/// about neither gets the head — and `markdownlint.mjs` then turns every remaining line break into a
/// space.
/// Every line break in a quoted context becomes a space, which `markdownlint.mjs` does to one before
/// it reaches the output — including one a rule passed to `addError` rather than `addErrorContext`,
/// which is the only way MD052's long labels stay on a single line.
pub(crate) fn one_line(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', " ")
}

pub(crate) fn ellipsify(text: &str, start: bool, end: bool) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let chars: Vec<char> = text.chars().collect();
    let shortened = if chars.len() <= 30 {
        text
    } else {
        let head = |count: usize| chars.iter().take(count).collect::<String>();
        let tail = |count: usize| chars.iter().skip(chars.len() - count).collect::<String>();
        if start && end {
            format!("{}...{}", head(15), tail(15))
        } else if end {
            format!("...{}", tail(30))
        } else {
            format!("{}...", head(30))
        }
    };
    shortened.replace('\n', " ")
}

/// markdownlint reads an absent setting as its documented default, which for the flags that turn a
/// style or a scope on is the opposite of `bool::default`.
pub(crate) fn default_on() -> bool {
    true
}

/// A bracketed group's contents, with `to` the index of the closing `]` rather than one past it.
#[derive(Clone, Copy)]
pub(crate) struct Label {
    pub from: usize,
    pub to: usize,
}

/// Where a `list_marker_*` node's glyph starts and how many bytes it covers.
///
/// The node spans the whole prefix — indentation, glyph and the whitespace after it — because
/// [`crate::rules::md023`] needs its end to be the item's content column. micromark's
/// `listItemMarker` is only the glyph, so a rule that reports on the marker itself has to narrow it.
/// The width comes from the node's kind rather than from trimming its text: a tab after the marker
/// makes the span reach *past* the content, so the text holds more than the prefix.
pub(crate) fn marker_glyph(marker: crate::ast::Node, text: &str) -> (usize, usize) {
    let trimmed = text.trim_start();
    let start = marker.start_position().column + (text.len() - trimmed.len());
    let len = match marker.kind() {
        "list_marker_dot" | "list_marker_parenthesis" => {
            trimmed.bytes().take_while(u8::is_ascii_digit).count() + 1
        }
        _ => 1,
    };
    (start, len)
}

/// The label of a link or image — what sits between the `[` the node starts at and its matching `]`.
/// Counted rather than taken from the node's children, because a label of nothing but whitespace has
/// no children and one holding a code span has several.
pub(crate) fn label_span(node: crate::ast::Node, source: &str) -> Option<Label> {
    let bytes = source.as_bytes();
    let mut open = node.start_byte();
    if bytes.get(open) == Some(&b'!') {
        open += 1;
    }
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let close = closing_bracket(bytes, open)?;
    Some(Label {
        from: open + 1,
        to: close,
    })
}

/// The index of the `]` closing the `[` at `open`, counting nested brackets and skipping whatever a
/// backslash escapes.
pub(crate) fn closing_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = open;
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
                    return Some(at - 1);
                }
            }
            _ => at += 1,
        }
    }
    None
}

#[derive(Debug)]
pub struct Rule {
    pub id: &'static str,
    /// Every name markdownlint gives the rule, in the order it prints them. Almost every rule has
    /// one; MD025 is also `single-title` and MD041 is also `first-line-h1`, and a config written for
    /// markdownlint may spell either.
    pub aliases: &'static [&'static str],
    pub tags: &'static [&'static str],
    pub description: &'static str,
    pub rule_type: RuleType,
    pub required_nodes: &'static [&'static str], // For caching optimization
    pub new_linter: fn(Rc<Context>) -> Box<dyn RuleLinter>,
}

impl Rule {
    /// The name the rest of quickmark keys on, which is the first one markdownlint prints.
    pub fn alias(&self) -> &'static str {
        self.aliases[0]
    }

    /// How markdownlint names a rule in its output: the id and then every alias, slash-separated, so
    /// `MD025/single-title/single-h1`.
    pub fn qualified_name(&self) -> String {
        let mut name = self.id.to_string();
        for alias in self.aliases {
            name.push('/');
            name.push_str(alias);
        }
        name
    }

    /// The severity a config gives this rule, under whichever of its names the config spelled it.
    ///
    /// `normalize_severities` folds a rule's names onto its first, but a config assembled by hand —
    /// as every rule test does — has not been through it, so each name is tried.
    pub fn severity_in<'a>(
        &self,
        severities: &'a HashMap<String, RuleSeverity>,
    ) -> Option<&'a RuleSeverity> {
        self.aliases.iter().find_map(|alias| severities.get(*alias))
    }
}

pub const ALL_RULES: &[Rule] = &[
    md001::MD001,
    md003::MD003,
    md004::MD004,
    md005::MD005,
    md007::MD007,
    md009::MD009,
    md010::MD010,
    md011::MD011,
    md012::MD012,
    md013::MD013,
    md014::MD014,
    md018::MD018,
    md019::MD019,
    md020::MD020,
    md021::MD021,
    md022::MD022,
    md023::MD023,
    md024::MD024,
    md025::MD025,
    md026::MD026,
    md027::MD027,
    md028::MD028,
    md029::MD029,
    md030::MD030,
    md031::MD031,
    md032::MD032,
    md033::MD033,
    md034::MD034,
    md035::MD035,
    md036::MD036,
    md037::MD037,
    md038::MD038,
    md039::MD039,
    md040::MD040,
    md041::MD041,
    md042::MD042,
    md043::MD043,
    md044::MD044,
    md045::MD045,
    md046::MD046,
    md047::MD047,
    md048::MD048,
    md049::MD049,
    md050::MD050,
    md051::MD051,
    md052::MD052,
    md053::MD053,
    md054::MD054,
    md055::MD055,
    md056::MD056,
    md058::MD058,
    md059::MD059,
    md060::MD060,
];
