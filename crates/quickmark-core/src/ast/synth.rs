//! The pieces of a tree-sitter-md tree that comrak does not produce, re-derived from raw source.
//!
//! Every span here was measured against tree-sitter-md 0.5.3 rather than inferred from comrak's
//! grammar, and `snapshot.rs` is what keeps them from drifting. Two of comrak's behaviours make this
//! necessary rather than merely convenient: it detaches paragraphs that turn out to be pure link
//! reference definitions, and it autocompletes table cells to the header width while dropping the
//! delimiter row — so neither can be read off its tree.

use comrak::nodes::{ListDelimType, ListType, NodeList};

use super::Kind;

/// Byte offset of every line, plus the per-line column helpers the synthesis recipes need.
///
/// Lines split on `\r\n`, a bare `\r` and `\n`, which is CommonMark's definition of a line ending
/// and what comrak counts. Agreeing with comrak here is not optional: a node's byte range is derived
/// from its line and column, so a line index that counts fewer lines than the parser does turns
/// every byte offset after the first bare `\r` into garbage. A `\r\n` line's span still keeps its
/// `\r` outside the content, so columns stay the UTF-8 byte offsets tree-sitter reported.
pub struct LineIndex<'a> {
    source: &'a str,
    starts: Vec<u32>,
    /// Byte offset of each line's terminator, i.e. its content end.
    ends: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    pub fn new(source: &'a str) -> Self {
        let bytes = source.as_bytes();
        let mut starts = vec![0u32];
        let mut ends = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' => {
                    ends.push(index as u32);
                    index += if bytes.get(index + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    starts.push(index as u32);
                }
                b'\n' => {
                    ends.push(index as u32);
                    index += 1;
                    starts.push(index as u32);
                }
                _ => index += 1,
            }
        }
        ends.push(bytes.len() as u32);
        Self {
            source,
            starts,
            ends,
        }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Whether the source ends in a line terminator, which is what makes [`Self::line_count`] count
    /// one more row than the source has content in.
    pub fn ends_with_line_terminator(&self) -> bool {
        self.source.ends_with(['\n', '\r'])
    }

    pub fn has_line(&self, row: usize) -> bool {
        row < self.starts.len()
    }

    /// Byte offset of `row`'s first byte. Rows past the end clamp to the source length, which is
    /// what makes a block end of `(line_count, 0)` resolve to EOF.
    pub fn line_start_byte(&self, row: usize) -> u32 {
        self.starts
            .get(row)
            .copied()
            .unwrap_or(self.source.len() as u32)
    }

    /// Byte offset just past `row`'s content, excluding its terminator.
    pub fn content_end(&self, row: usize) -> u32 {
        self.ends
            .get(row)
            .copied()
            .unwrap_or(self.source.len() as u32)
    }

    /// Width of `row`'s content in bytes, excluding its line terminator.
    pub fn content_len(&self, row: usize) -> u32 {
        self.content_end(row) - self.line_start_byte(row)
    }

    pub fn content(&self, row: usize) -> &'a str {
        &self.source[self.line_start_byte(row) as usize..self.content_end(row) as usize]
    }

    /// The source of rows `from` through `to` inclusive, each with its terminator. This is what a
    /// block's tail is handed to a nested parse as a document of its own.
    pub fn rows(&self, from: usize, to: usize) -> &'a str {
        &self.source[self.line_start_byte(from) as usize..self.line_start_byte(to + 1) as usize]
    }

    pub fn byte_at(&self, row: usize, column: usize) -> u32 {
        self.line_start_byte(row) + column as u32
    }

    /// Column of `row`'s first byte that is neither a space nor a tab.
    pub fn first_non_space_col(&self, row: usize) -> u32 {
        let text = self.content(row);
        let skip = text.len() - text.trim_start_matches([' ', '\t']).len();
        skip as u32
    }

    /// Column just past `row`'s last byte that is neither a space nor a tab.
    pub fn trim_end_col(&self, row: usize) -> u32 {
        let text = self.content(row);
        text.trim_end_matches([' ', '\t']).len() as u32
    }

    /// Where an `inline` node ends on `row`.
    ///
    /// tree-sitter-md drops trailing whitespace before a newline but keeps it at end of file, so
    /// `hello   \n` has a five-column inline while `hello   ` has an eight-column one. Preserving
    /// that asymmetry is what keeps `**x** ` distinguishable from `**x**` without breaking headings
    /// in files that do not end in a newline.
    pub fn inline_end_col(&self, row: usize) -> u32 {
        if self.has_line(row + 1) {
            self.trim_end_col(row)
        } else {
            self.content_len(row)
        }
    }

    /// A block's end under tree-sitter-md's convention of swallowing the trailing newline: column 0
    /// of the following line, or the content end when there is no following line.
    pub fn block_end_row(&self, last_content_row: u32) -> (u32, u32) {
        if self.has_line(last_content_row as usize + 1) {
            (last_content_row + 1, 0)
        } else {
            (
                last_content_row,
                self.content_len(last_content_row as usize),
            )
        }
    }
}

/// A half-open row/column span: `start` inclusive, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    start: (u32, u32),
    end: (u32, u32),
}

impl Span {
    pub fn new(start: (u32, u32), end: (u32, u32)) -> Self {
        Self { start, end }
    }

    pub fn start(self) -> (u32, u32) {
        self.start
    }

    pub fn end(self) -> (u32, u32) {
        self.end
    }

    /// The last row carrying any of this span's content. A span ending at column 0 stops before its
    /// end row begins.
    pub fn last_row(self) -> u32 {
        if self.end.1 == 0 {
            self.end.0.saturating_sub(1)
        } else {
            self.end.0
        }
    }

    pub fn contains_row(self, row: u32) -> bool {
        self.start.0 <= row && row <= self.last_row()
    }
}

/// The column a block quote's content starts at, given the column its `>` occupies.
///
/// Up to three spaces, the `>`, then one optional space — the same prefix CommonMark strips from
/// every line of the quote, and the column tree-sitter-md gives the blocks inside it.
pub fn block_quote_content_col(lines: &LineIndex<'_>, row: u32, start_col: u32) -> u32 {
    let bytes = lines.content(row as usize).as_bytes();
    let mut col = start_col as usize;
    let mut spaces = 0;
    while spaces < 3 && matches!(bytes.get(col), Some(b' ') | Some(b'\t')) {
        col += 1;
        spaces += 1;
    }
    if bytes.get(col) != Some(&b'>') {
        return start_col;
    }
    col += 1;
    if matches!(bytes.get(col), Some(b' ') | Some(b'\t')) {
        col += 1;
    }
    col as u32
}

/// A synthesized marker node: which kind it is and where it sits.
pub struct Marker {
    pub kind: Kind,
    pub span: Span,
}

/// comrak handles only one front matter delimiter, and quickmark needs `---`. A `+++` block is found
/// here instead and collapsed into a single `plus_metadata` node by the builder. Without it the
/// block parses as a paragraph, which is harmless, but as a thematic break plus a setext heading it
/// would trip MD041 into reporting a missing top-level heading.
pub fn plus_front_matter(source: &str, lines: &LineIndex<'_>) -> Option<Span> {
    if !source.starts_with("+++") {
        return None;
    }
    if lines.content(0).trim_end() != "+++" {
        return None;
    }
    let close = (1..lines.line_count()).find(|&row| lines.content(row).trim() == "+++")?;
    Some(Span::new((0, 0), lines.block_end_row(close as u32)))
}

/// The `$$…$$` regions of a document, one span each, in document order.
///
/// markdownlint's micromark folds these into a single `mathFlow` token that swallows everything
/// between the fences, so a `# B` inside one is not a heading and a `<b>` inside one is not inline
/// HTML. No Rust Markdown parser does — comrak, pulldown-cmark and tree-sitter-md all hand back the
/// blocks the LaTeX happens to look like — so the regions are found in the raw source and whatever
/// comrak built inside them is dropped.
///
/// Every rule here was measured against markdownlint-cli2 v0.23.3, and then checked against
/// micromark's `math-flow.js`, which agrees:
///
/// - The opener is a run of two or more `$` at the start of a line, after up to three spaces of
///   indentation and any block quote or list markers. Four spaces makes it an indented code block
///   instead.
/// - Whatever follows the run on that line is micromark's *meta*, and a `$` anywhere in the meta
///   makes the whole construct fail (`function meta`: `if (code === 36) return nok(code)`). So
///   `$$ x $$` is not a one-line block — it is a paragraph whose `$$x$$` the *inline* tokenizer
///   folds into math, and anything after it on the line is ordinary content. `$$$$` is a maximal run
///   with empty meta, so it does open.
/// - A later line closes the block only when it is a run of at least the opener's length followed by
///   nothing but whitespace. `$$ z` therefore does not close, and `$$$` closes a `$$` block while
///   `$$` does not close a `$$$` one.
/// - A block that is never closed runs to the end of the document. Blank lines do not end it.
/// - An opener does interrupt a paragraph that is already open, so `text` followed by `$$` on the
///   next line starts a region.
///
/// One shape is known not to work: `text` immediately followed by `$$` with no blank line between
/// keeps one comrak paragraph spanning both lines, so nothing *starts* inside the region and no
/// `math_block` is emitted; splitting the paragraph would mean rewriting a comrak block's range
/// mid-emission.
///
/// `containers` holds the first and last row of every block quote, list and list item in the
/// document, because micromark's mathFlow dies with the container it opened in and a line-only scan
/// cannot see one. Clipping has to happen *while* scanning rather than after it: the scan resumes
/// past the region it just found, and resuming past an unclipped closer steps over a `$$` that
/// micromark reads as the next region's opener.
pub fn math_regions(lines: &LineIndex<'_>, containers: &[(u32, u32)]) -> Vec<Span> {
    let mut regions = Vec::new();
    let mut row = 0usize;
    while row < lines.line_count() {
        let Some((column, dollars)) = dollar_run(lines, row, 3) else {
            row += 1;
            continue;
        };
        if lines.content(row)[column + dollars..]
            .trim_start()
            .contains('$')
        {
            row += 1;
            continue;
        }

        let close = (row + 1..lines.line_count()).find(|&candidate| {
            dollar_run(lines, candidate, column + 3).is_some_and(|(at, run)| {
                run >= dollars && lines.content(candidate)[at + run..].trim().is_empty()
            })
        });
        let last = close.unwrap_or(lines.line_count() - 1);
        let last = clip_to_container(row, last, containers);
        regions.push(Span::new(
            (row as u32, column as u32),
            lines.block_end_row(last as u32),
        ));
        row = last + 1;
    }
    regions
}

/// The last row a region opened on `opener` reaches. Containers holding a row form a chain, so the
/// one that ends soonest is the innermost, and a region cannot outlive it.
fn clip_to_container(opener: usize, last: usize, containers: &[(u32, u32)]) -> usize {
    containers
        .iter()
        .filter(|&&(first, end)| first <= opener as u32 && opener as u32 <= end)
        .map(|&(_, end)| end as usize)
        .min()
        .map_or(last, |end| last.min(end))
}

/// The `$…$` spans micromark's math extension forms over `source[from..to]`, as absolute byte ranges.
///
/// micromark pairs *runs* of `$`, not individual ones, and asks nothing of the whitespace around
/// them, which is looser than comrak's `math_dollars`: comrak follows pandoc, needing the opening
/// `$` followed by a non-space and the closing one preceded by a non-space. So `$ a $` is math for
/// markdownlint and ordinary text for comrak, and a URL inside it is not a bare URL.
///
/// A run closes the one that opened it only when the two are the same length; a run of any other
/// length is content and the search carries on past it, so `$a$$b$` is one span rather than two. An
/// opening run nothing closes produces nothing at all.
///
/// `claimed` holds the ranges already accounted for — code spans, and any other span whose `$` is
/// literal — because a `$` inside one delimits nothing.
pub fn inline_math_spans(
    source: &str,
    from: usize,
    to: usize,
    claimed: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let to = to.min(bytes.len());
    let literal = |at: usize| {
        bytes[at] != b'$'
            || escaped_dollar(bytes, at)
            || claimed.iter().any(|&(start, end)| start <= at && at < end)
    };

    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut at = from.min(to);
    while at < to {
        if literal(at) {
            at += 1;
            continue;
        }
        let mut end = at;
        while end < to && !literal(end) {
            end += 1;
        }
        runs.push((at, end));
        at = end;
    }

    let mut spans = Vec::new();
    let mut index = 0;
    while index < runs.len() {
        let (open, open_end) = runs[index];
        let closer = runs[index + 1..]
            .iter()
            .position(|&(start, end)| end - start == open_end - open);
        match closer {
            Some(offset) => {
                spans.push((open, runs[index + 1 + offset].1));
                index += 2 + offset;
            }
            // The attempt fails and the whole opening run is spent, so scanning resumes past it.
            None => index += 1,
        }
    }
    spans
}

/// Whether the `$` at `at` is preceded by an odd number of backslashes, which makes it literal.
fn escaped_dollar(bytes: &[u8], at: usize) -> bool {
    let mut backslashes = 0;
    let mut index = at;
    while index > 0 && bytes[index - 1] == b'\\' {
        backslashes += 1;
        index -= 1;
    }
    backslashes % 2 == 1
}

/// The column a line's `$` run starts at and how long it is — or `None` when the line does not open
/// with a run of at least two `$` once [`content_column`] has skipped the container prefixes.
///
/// For a region's *closer*, `indent` is the column the region opened at plus three, because inside a
/// list item the whole block sits that much further right and a closer indented four from the margin
/// is only two past the item's content column. Only the opener's own line carries a list marker; the
/// closer lines that follow are indented to the item's content column and start with their `$` run
/// directly.
fn dollar_run(lines: &LineIndex<'_>, row: usize, indent: usize) -> Option<(usize, usize)> {
    let bytes = lines.content(row).as_bytes();
    let start = content_column(bytes, indent);
    let mut column = start;
    while bytes.get(column) == Some(&b'$') {
        column += 1;
    }
    (column - start >= 2).then_some((start, column - start))
}

/// Where a line's own content begins, once up to `indent` spaces of indentation, any block quote
/// markers and one list marker have been skipped.
///
/// `indent` is how many leading spaces may precede the content. Three at document level, where four
/// would make the line an indented code block; for something inside a list item it is the column the
/// item's content starts at plus three, because a line indented four from the margin is only two past
/// the item's content column.
///
/// Only a container's *first* line carries its list marker; the lines that follow are indented to the
/// item's content column and start with their own content directly.
pub(crate) fn content_column(bytes: &[u8], indent: usize) -> usize {
    let mut column = 0usize;
    let mut limit = indent;
    loop {
        let mut spaces = 0;
        while spaces < limit && bytes.get(column) == Some(&b' ') {
            column += 1;
            spaces += 1;
        }
        limit = 3;
        match bytes.get(column) {
            Some(b'>') => {
                column += 1;
                if bytes.get(column) == Some(&b' ') {
                    column += 1;
                }
            }
            Some(b'-' | b'*' | b'+') => match skip_marker_spaces(bytes, column + 1) {
                Some(after) => column = after,
                None => break,
            },
            Some(b'0'..=b'9') => {
                let mut at = column;
                let mut digits = 0;
                while digits < 9 && matches!(bytes.get(at), Some(b'0'..=b'9')) {
                    at += 1;
                    digits += 1;
                }
                if !matches!(bytes.get(at), Some(b'.') | Some(b')')) {
                    break;
                }
                match skip_marker_spaces(bytes, at + 1) {
                    Some(after) => column = after,
                    None => break,
                }
            }
            _ => break,
        }
    }
    column
}

/// The column a list item's content starts at, given the column just after its marker. One to four
/// spaces; none at all, or five and up — which makes the item's first block an indented code block —
/// means this was not a list marker.
fn skip_marker_spaces(bytes: &[u8], mut at: usize) -> Option<usize> {
    let start = at;
    while bytes.get(at) == Some(&b' ') {
        at += 1;
    }
    (at > start && at - start <= 4).then_some(at)
}

/// The two children of an ATX heading: its `#` run and, when there is any, its text.
pub struct AtxParts {
    pub marker: Marker,
    pub inline: Option<Span>,
}

/// Splits an ATX heading line at `start_col` into its marker and its text.
///
/// The marker absorbs the line's indentation, so `   ### Deep` has a six-byte `atx_h3_marker`, while
/// a heading inside a block quote starts at its own `#`. The text span does **not** chop a closing
/// `#` sequence — `# H1 #` has a four-byte `inline` reading `H1 #` — but it does stop before
/// trailing whitespace. A heading with no text gets no `inline` at all, which is what makes MD019
/// see `##` as a marker with nothing after it.
pub fn atx_parts(lines: &LineIndex<'_>, row: u32, start_col: u32) -> AtxParts {
    let text = lines.content(row as usize);
    let bytes = text.as_bytes();
    let hashes_start = (start_col as usize..bytes.len())
        .find(|&offset| bytes[offset] == b'#')
        .unwrap_or(bytes.len()) as u32;
    let mut hashes_end = hashes_start;
    while (hashes_end as usize) < bytes.len() && bytes[hashes_end as usize] == b'#' {
        hashes_end += 1;
    }

    let after = text.get(hashes_end as usize..).unwrap_or("");
    let skip = (after.len() - after.trim_start_matches([' ', '\t']).len()) as u32;
    let content_start = hashes_end + skip;
    let content_end = chop_closing_hashes(bytes, lines.inline_end_col(row as usize) as usize);

    AtxParts {
        marker: Marker {
            kind: atx_marker_kind((hashes_end - hashes_start).clamp(1, 6) as u8),
            span: Span::new((row, start_col), (row, hashes_end)),
        },
        inline: (content_start < content_end as u32)
            .then_some(Span::new((row, content_start), (row, content_end as u32))),
    }
}

/// The column a closed ATX heading's text ends at, mirroring comrak's
/// `strings::chop_trailing_hashes`: a trailing `#` run preceded by a space or a tab is a closing
/// sequence, and it and the whitespace before it are not part of the heading. `# H#` ends in `H#`
/// because the run is not preceded by whitespace, and a run reaching the start of the line is the
/// whole heading rather than a closing one.
fn chop_closing_hashes(bytes: &[u8], content_end: usize) -> usize {
    if content_end == 0 {
        return 0;
    }
    let mut at = content_end - 1;
    while bytes[at] == b'#' {
        if at == 0 {
            return content_end;
        }
        at -= 1;
    }
    if at == content_end - 1 || !matches!(bytes[at], b' ' | b'\t') {
        return content_end;
    }
    (0..at)
        .rev()
        .find(|&index| !matches!(bytes[index], b' ' | b'\t'))
        .map_or(0, |index| index + 1)
}

fn atx_marker_kind(level: u8) -> Kind {
    match level {
        1 => Kind::AtxH1Marker,
        2 => Kind::AtxH2Marker,
        3 => Kind::AtxH3Marker,
        4 => Kind::AtxH4Marker,
        5 => Kind::AtxH5Marker,
        _ => Kind::AtxH6Marker,
    }
}

/// The `===` or `---` under a setext heading. Unlike an `inline`, the underline keeps its trailing
/// whitespace: `===   ` spans all six columns.
pub fn setext_underline(lines: &LineIndex<'_>, row: u32, level: u8) -> Marker {
    Marker {
        kind: if level == 1 {
            Kind::SetextH1Underline
        } else {
            Kind::SetextH2Underline
        },
        span: Span::new((row, 0), (row, lines.content_len(row as usize))),
    }
}

/// A list item's marker, including the whitespace between it and the content. `content_col` is
/// where the item's first child starts; tree-sitter-md's marker runs all the way to it, so `-   one`
/// has a four-byte `list_marker_minus`.
pub fn list_marker(
    lines: &LineIndex<'_>,
    start: (u32, u32),
    content_col: Option<u32>,
    list: &NodeList,
) -> Marker {
    let (kind, text_len) = match list.list_type {
        ListType::Bullet => (
            match list.bullet_char {
                b'+' => Kind::ListMarkerPlus,
                b'*' => Kind::ListMarkerStar,
                _ => Kind::ListMarkerMinus,
            },
            1,
        ),
        ListType::Ordered => (
            if list.delimiter == ListDelimType::Paren {
                Kind::ListMarkerParenthesis
            } else {
                Kind::ListMarkerDot
            },
            // The digits are part of the marker: MD029 parses them as the item's ordinal and MD005
            // measures the marker's width.
            ordered_marker_len(lines, start),
        ),
    };
    // Clamped to the line's content: a bare `-` with nothing after it has a one-byte marker, not a
    // two-byte one.
    let end_col = content_col
        .unwrap_or(start.1 + text_len + 1)
        .min(lines.content_len(start.0 as usize));
    Marker {
        kind,
        span: Span::new(start, (start.0, end_col.max(start.1 + text_len))),
    }
}

fn ordered_marker_len(lines: &LineIndex<'_>, start: (u32, u32)) -> u32 {
    let text = lines.content(start.0 as usize);
    let from = text.get(start.1 as usize..).unwrap_or("");
    let digits = from.chars().take_while(|c| c.is_ascii_digit()).count();
    (digits + 1) as u32
}

/// The content of a fenced code block: everything between the fences, ending on the closing fence's
/// line. Absent for an empty fence, which is why the option.
///
/// Both endpoints sit at `block_start_col` rather than at column 0. At the document level that column
/// is 0, but a fence indented inside a list item starts at the item's content column and its content
/// node starts and ends there too.
pub fn code_fence_content(
    block_start_row: u32,
    block_start_col: u32,
    block_end: (u32, u32),
    closed: bool,
) -> Option<Span> {
    let first = block_start_row + 1;
    // A closed block's end row is the closing fence; an unclosed one runs to EOF.
    let last_content_row = if closed {
        let end_row = if block_end.1 == 0 {
            block_end.0.saturating_sub(1)
        } else {
            block_end.0
        };
        end_row.saturating_sub(1)
    } else if block_end.1 == 0 {
        block_end.0.saturating_sub(1)
    } else {
        block_end.0
    };
    (last_content_row >= first).then_some(Span::new(
        (first, block_start_col),
        (last_content_row + 1, block_start_col),
    ))
}

/// One row of a pipe table, with its cells and its bare `|` separators in column order.
pub struct RowSpan {
    pub kind: Kind,
    pub span: Span,
    pub children: Vec<RowChild>,
}

/// A cell or a pipe, carrying the column it starts at so the row's children can be sorted into
/// source order — tree-sitter interleaves them and MD056/MD060 walk them in that order.
pub struct RowChild {
    pub column: u32,
    pub kind: Kind,
    pub span: Span,
}

/// The rows of a pipe table, derived from the raw lines.
///
/// comrak cannot supply these: it consumes the delimiter row into `Table::alignments` without
/// adding it to the tree, pads every body row up to the header width with empty cells (which would
/// stop MD056 ever firing), and reports cell spans that include the surrounding padding where
/// tree-sitter-md's exclude it.
///
/// `prefix` is the column the table's container's content starts at — 0 at document level, past the
/// `> ` of a block quote, past a list item's marker. Rows are measured from it, not from the line
/// start, so a table inside a container does not report its indentation as leading padding.
pub fn table_rows(
    lines: &LineIndex<'_>,
    prefix: u32,
    header_row: u32,
    body_rows: &[u32],
) -> Vec<RowSpan> {
    let mut rows = Vec::with_capacity(body_rows.len() + 2);
    rows.push((header_row, Kind::PipeTableHeader, Kind::PipeTableCell, true));
    rows.push((
        header_row + 1,
        Kind::PipeTableDelimiterRow,
        Kind::PipeTableDelimiterCell,
        false,
    ));
    for &body in body_rows {
        rows.push((body, Kind::PipeTableRow, Kind::PipeTableCell, false));
    }

    rows.into_iter()
        .filter(|&(row, _, _, _)| lines.has_line(row as usize))
        .map(|(row, kind, cell_kind, is_header)| {
            // Only the header row starts at its first non-space; the delimiter and body rows start
            // at the container's content column. Both end at the line's last non-blank byte, so
            // trailing whitespace is outside the row.
            let start_col = if is_header {
                lines.first_non_space_col(row as usize).max(prefix)
            } else {
                prefix
            };
            let end_col = lines.trim_end_col(row as usize);
            let mut children = split_row(lines, row, prefix, cell_kind);
            children.sort_by_key(|child| child.column);
            RowSpan {
                kind,
                span: Span::new((row, start_col), (row, end_col)),
                children,
            }
        })
        .collect()
}

/// Splits one table line into cells and `|` separators, ignoring the first `prefix` columns, which
/// belong to the enclosing container.
///
/// A cell is left-trimmed but not right-trimmed — `| a | b |` yields cells `a ` and `b ` — except
/// when it holds nothing but whitespace, in which case it keeps the whole inter-pipe region. A
/// zero-width cell (`||`) produces nothing. A `|` preceded by an odd run of backslashes is escaped
/// and separates nothing.
fn split_row(lines: &LineIndex<'_>, row: u32, prefix: u32, cell_kind: Kind) -> Vec<RowChild> {
    let text = lines.content(row as usize);
    let bytes = text.as_bytes();
    let width = lines.content_len(row as usize);

    let mut pipes = Vec::new();
    for (offset, &byte) in bytes.iter().enumerate() {
        if byte == b'|' && !is_escaped(bytes, offset) {
            pipes.push(offset as u32);
        }
    }

    let mut children = Vec::new();
    // `(start, end, is_edge)`. The regions outside the outermost pipes are edges: they hold a cell
    // only when a table omits its leading or trailing pipe. The whitespace either side of `| a |` is
    // an edge region and produces nothing, where the whitespace inside `| a |  |` is interior and
    // produces an empty cell. The leading edge starts at `prefix` so a block quote's `> ` is not
    // mistaken for a cell.
    let mut regions: Vec<(u32, u32, bool)> = Vec::new();
    match pipes.first() {
        None => regions.push((prefix, width, true)),
        Some(&first) => {
            if first > prefix {
                regions.push((prefix, first, true));
            }
            for pair in pipes.windows(2) {
                regions.push((pair[0] + 1, pair[1], false));
            }
            let after_last = pipes[pipes.len() - 1] + 1;
            if after_last < width {
                regions.push((after_last, width, true));
            }
        }
    }

    // A delimiter cell matches `[-:]+` exactly, so it is trimmed on both sides. A content cell is
    // only left-trimmed: `| a | b |` yields cells `a ` and `b `, trailing space included.
    let trim_right = cell_kind == Kind::PipeTableDelimiterCell;
    for &(start, end, is_edge) in &regions {
        if start >= end {
            continue;
        }
        let segment = &text[start as usize..end as usize];
        let skip = (segment.len() - segment.trim_start_matches([' ', '\t']).len()) as u32;
        if skip == segment.len() as u32 {
            // Nothing but whitespace. An interior cell keeps the whole region; an edge is padding.
            if is_edge {
                continue;
            }
            children.push(RowChild {
                column: start,
                kind: cell_kind,
                span: Span::new((row, start), (row, end)),
            });
            continue;
        }
        let cell_start = start + skip;
        let cell_end = if trim_right {
            end - (segment.len() - segment.trim_end_matches([' ', '\t']).len()) as u32
        } else {
            end
        };
        children.push(RowChild {
            column: cell_start,
            kind: cell_kind,
            span: Span::new((row, cell_start), (row, cell_end)),
        });
    }

    for pipe in pipes {
        children.push(RowChild {
            column: pipe,
            kind: Kind::Pipe,
            span: Span::new((row, pipe), (row, pipe + 1)),
        });
    }
    children
}

fn is_escaped(bytes: &[u8], offset: usize) -> bool {
    let mut backslashes = 0;
    while backslashes < offset && bytes[offset - backslashes - 1] == b'\\' {
        backslashes += 1;
    }
    backslashes % 2 == 1
}

/// Whether a line opens a link reference definition. Used both to find the definitions comrak
/// detached and to split the leading ones off a paragraph comrak kept, which it does when prose
/// follows on the next line. Block quote markers are skipped first, so `> [a]: /u` counts.
pub fn is_link_reference_definition(lines: &LineIndex<'_>, row: usize) -> bool {
    let content = lines.content(row);
    let prefix = (container_prefix_width(lines, row as u32) as usize).min(content.len());
    is_definition_line(&content[prefix..])
}

/// One line of a definition: either a link reference definition — up to three spaces of indent, a
/// label, a colon, a destination, and then either the end of the line or a title and the end of the
/// line — or a GFM footnote definition, whose `[^name]:` is followed by arbitrary markdown.
///
/// The destination-and-title rule is why this is a parser and not a `[label]:` pattern: prose where
/// only a title may go makes the whole thing an ordinary paragraph. A footnote definition has no such
/// constraint, and comrak drops every definition nothing refers to, so without the `[^name]:` branch
/// an unused footnote would reach no rule at all — MD053 could not report it and MD013 would measure
/// it instead of exempting it.
fn is_definition_line(text: &str) -> bool {
    let bytes = text.as_bytes();
    let indent = bytes.iter().take_while(|&&byte| byte == b' ').count();
    if indent > 3 || bytes.get(indent) != Some(&b'[') {
        return false;
    }
    if is_footnote_definition_line(bytes, indent) {
        return true;
    }

    let mut at = indent + 1;
    let label_start = at;
    loop {
        match bytes.get(at) {
            None => return false,
            Some(b'\\') => at += 2,
            Some(b']') => break,
            Some(_) => at += 1,
        }
    }
    if at == label_start || bytes.get(at + 1) != Some(&b':') {
        return false;
    }

    at = skip_space_tab(bytes, at + 2);
    let (after_destination, balanced) = destination(bytes, at);
    if !balanced {
        return false;
    }
    at = skip_space_tab(bytes, after_destination);
    if at >= bytes.len() {
        return true;
    }

    let (after_title, closed) = match title(bytes, at) {
        Some(found) => found,
        // Not a title, so there is trailing content after the destination.
        None => return false,
    };
    // An unterminated title continues on the following lines, which `reference_definitions` absorbs.
    !closed || skip_space_tab(bytes, after_title) >= bytes.len()
}

/// `[^name]:` starting at `indent`, where GFM lets the name be anything but whitespace and `]`.
fn is_footnote_definition_line(bytes: &[u8], indent: usize) -> bool {
    let Some(b'^') = bytes.get(indent + 1) else {
        return false;
    };
    let name_start = indent + 2;
    let mut at = name_start;
    while let Some(&byte) = bytes.get(at) {
        if byte == b']' {
            break;
        }
        if byte.is_ascii_whitespace() {
            return false;
        }
        at += 1;
    }
    at > name_start && bytes.get(at) == Some(&b']') && bytes.get(at + 1) == Some(&b':')
}

fn skip_space_tab(bytes: &[u8], mut at: usize) -> usize {
    while matches!(bytes.get(at), Some(b' ') | Some(b'\t')) {
        at += 1;
    }
    at
}

/// The end of a link destination, and whether it was well formed. Returns the offset just past it.
fn destination(bytes: &[u8], mut at: usize) -> (usize, bool) {
    if bytes.get(at) == Some(&b'<') {
        at += 1;
        loop {
            match bytes.get(at) {
                None | Some(b'<') => return (at, false),
                Some(b'\\') => at += 2,
                Some(b'>') => return (at + 1, true),
                Some(_) => at += 1,
            }
        }
    }

    let start = at;
    let mut depth = 0usize;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b' ' | b'\t' => break,
            b'(' => {
                depth += 1;
                at += 1;
            }
            b')' => {
                let Some(open) = depth.checked_sub(1) else {
                    return (at, false);
                };
                depth = open;
                at += 1;
            }
            b'<' | b'>' => return (at, false),
            _ => at += 1,
        }
    }
    (at, at > start && depth == 0)
}

/// A link title's end and whether its closing delimiter appeared on this line.
fn title(bytes: &[u8], at: usize) -> Option<(usize, bool)> {
    let close = match bytes.get(at) {
        Some(b'"') | Some(b'\'') => bytes[at],
        Some(b'(') => b')',
        _ => return None,
    };
    let mut index = at + 1;
    loop {
        match bytes.get(index) {
            None => return Some((index, false)),
            Some(b'\\') => index += 2,
            Some(&byte) if byte == close => return Some((index + 1, true)),
            Some(_) => index += 1,
        }
    }
}

/// The link reference definitions comrak detached, one span each.
///
/// Detection is by line shape rather than by "which lines are left over", so an empty list item or a
/// blank line inside a container cannot be mistaken for one. A definition absorbs the indented
/// continuation lines that carry its title, and stops at a blank line or at the next definition,
/// which is what makes `[a]: /u` and `[b]: /v` on consecutive lines two nodes rather than one.
pub fn reference_definitions(lines: &LineIndex<'_>, covered: &[bool]) -> Vec<Span> {
    let mut runs = Vec::new();
    let mut open: Option<usize> = None;
    for row in 0..lines.line_count() {
        let blank = lines.content(row).trim().is_empty();
        let claimed = covered.get(row).copied().unwrap_or(true);
        let is_start = !claimed && !blank && is_link_reference_definition(lines, row);
        let continues = open.is_some() && !claimed && !blank && !is_start;
        if is_start {
            if let Some(first) = open.take() {
                runs.push(definition_span(lines, first, row - 1));
            }
            open = Some(row);
        } else if !continues {
            if let Some(first) = open.take() {
                runs.push(definition_span(lines, first, row - 1));
            }
        }
    }
    if let Some(first) = open {
        runs.push(definition_span(lines, first, lines.line_count() - 1));
    }
    runs
}

fn definition_span(lines: &LineIndex<'_>, first: usize, last: usize) -> Span {
    Span::new(
        (first as u32, container_prefix_width(lines, first as u32)),
        lines.block_end_row(last as u32),
    )
}

/// The column a definition starts at, skipping any block quote markers that precede it. Plain
/// indentation is *not* skipped: `  [a]: /u` has a definition at column 0, while `> [a]: /u` has one
/// at column 2.
fn container_prefix_width(lines: &LineIndex<'_>, row: u32) -> u32 {
    let text = lines.content(row as usize);
    let bytes = text.as_bytes();
    let mut col = 0usize;
    loop {
        match bytes.get(col) {
            Some(b' ') | Some(b'\t') => {
                let next = bytes[col..].iter().position(|&b| b != b' ' && b != b'\t');
                match next.map(|n| bytes[col + n]) {
                    Some(b'>') => col += next.unwrap_or(0),
                    _ => break,
                }
            }
            Some(b'>') => {
                col += 1;
                if bytes.get(col) == Some(&b' ') {
                    col += 1;
                }
            }
            _ => break,
        }
    }
    col as u32
}

#[cfg(test)]
mod tests {
    use super::{inline_math_spans, is_definition_line, math_regions, LineIndex};

    /// `$$…$$` region detection, one case per rule measured against markdownlint-cli2 v0.23.3 with a
    /// heading payload: a swallowed heading means the region covers it, a reported one means it does
    /// not. Spans are `(start row, start column)-(end row, end column)`, where the end follows the
    /// block convention of swallowing the trailing newline.
    #[test]
    fn math_regions_follow_the_measured_delimiter_rules() {
        type Regions = &'static [((u32, u32), (u32, u32))];
        let cases: &[(&str, Regions)] = &[
            // Opened and closed on their own lines.
            ("$$\nx\n$$\n", &[((0, 0), (3, 0))]),
            // Up to three spaces of indentation; four makes it an indented code block instead.
            ("   $$\nx\n   $$\n", &[((0, 3), (3, 0))]),
            ("    $$\nx\n    $$\n", &[]),
            // A `$` in the meta stops the block from opening at all, however many `$` follow it.
            ("$$ x $$\n", &[]),
            ("$$x$$\n", &[]),
            ("$$ x $\n", &[]),
            ("$$ a $$ b $$\n", &[]),
            // A maximal leading run leaves an empty meta, so `$$$$` does open.
            ("$$$$\n", &[((0, 0), (1, 0))]),
            // A later line closes only when it is a long-enough `$` run followed by nothing but
            // whitespace, so `$$ z` does not close and the block runs to the end of the document.
            ("$$\nx\n$$ z\n", &[((0, 0), (3, 0))]),
            // A longer run closes a shorter opener, but not the other way round.
            ("$$\nx\n$$$\n", &[((0, 0), (3, 0))]),
            ("$$$\nx\n$$\n", &[((0, 0), (3, 0))]),
            // One `$` is inline math, not a block.
            ("$\nx\n$\n", &[]),
            // The opener has to start the line; a `$` run further along one opens nothing, so the
            // heading before it is still a heading and the trailing `$$` opens a block of its own.
            ("# A\n\ntext $$\n# B\n$$\n", &[((4, 0), (5, 0))]),
            // Whatever follows a closed block is linted normally again.
            ("$$\n# B\n$$\n# C\n", &[((0, 0), (3, 0))]),
            (
                "$$\nx\n$$\n\n$$\ny\n$$\n",
                &[((0, 0), (3, 0)), ((4, 0), (7, 0))],
            ),
            // Block quote markers and list markers are stripped, and the region keeps the container's
            // content column so it lands inside the container rather than replacing it.
            ("> $$\n> x\n> $$\n", &[((0, 2), (3, 0))]),
            ("- $$\n  x\n  $$\n", &[((0, 2), (3, 0))]),
            ("1. $$\n   x\n   $$\n", &[((0, 3), (3, 0))]),
            // A closer is measured from where the region opened, not from the margin: inside a list
            // item indented two, a closer indented four from the margin is only two past the content.
            ("- a\n- $$\n  x\n  $$\n- b\n", &[((1, 2), (4, 0))]),
        ];

        for &(source, expected) in cases {
            let lines = LineIndex::new(source);
            let actual: Vec<((u32, u32), (u32, u32))> = math_regions(&lines, &[])
                .iter()
                .map(|span| (span.start(), span.end()))
                .collect();
            assert_eq!(actual, expected, "wrong regions for {source:?}");
        }
    }

    /// micromark's mathFlow dies with the container it opened in, and the scan has to resume where
    /// the *clipped* region ended. Resuming past the closer the line scan found instead steps over a
    /// `$$` micromark reads as the next region's opener, which then swallows real content.
    ///
    /// Both regions below are measured against micromark's token stream: `mathFlow L3:3-L5:1` inside
    /// the list item and `mathFlow L6:1-L8:3` at document level.
    #[test]
    fn a_clipped_region_does_not_swallow_the_next_opener() {
        // `- a` and its `$$`/`x` are one list item spanning rows 0..=3.
        let source = "- a\n\n  $$\n  x\n\n$$\ny\n$$\n- item\n";
        let lines = LineIndex::new(source);
        let regions = |containers: &[(u32, u32)]| {
            math_regions(&lines, containers)
                .iter()
                .map(|span| (span.start(), span.end()))
                .collect::<Vec<_>>()
        };
        assert_eq!(vec![((2, 2), (4, 0)), ((5, 0), (8, 0))], regions(&[(0, 3)]));
        // Unclipped, the opener pairs with the `$$` on row 5 and the scan never sees it again.
        assert_eq!(vec![((2, 2), (6, 0)), ((7, 0), (9, 0))], regions(&[]));
    }
    #[test]
    fn definition_shapes() {
        for line in [
            "[a]: /u",
            "   [a]: /u",
            "[a]: /u \"title\"",
            "[a]: /u 'title'",
            "[a]: /u (title)",
            "[a]:\t/u\t\"title\"",
            "[a]: <http://x.y/z> \"t\"",
            "[a]: /u(b)",
            // An angle-bracketed destination may be empty.
            "[a]: <>",
            "[a b]: /u",
            "[a\\]]: /u",
            // An unterminated title continues on the following lines, which `reference_definitions`
            // absorbs into the same node.
            "[a]: /u \"title",
            // A footnote definition takes arbitrary markdown where a link definition may only have a
            // destination and a title.
            "[^a]: prose here",
            "[^version]: It's generally good practice for Rust posts",
            "   [^a]: note",
            "[^a]:",
        ] {
            assert!(is_definition_line(line), "should be a definition: {line:?}");
        }
    }

    /// Prose where only a title may go makes a *link* reference definition an ordinary paragraph, so
    /// `[a]: /url ok` is not one. Getting this wrong invents a definition and costs a spurious
    /// MD052/MD053 report. A footnote definition is the exception: `[^a]:` takes arbitrary markdown,
    /// and comrak drops the ones nothing refers to, so this is the only way an unused footnote reaches
    /// MD053 at all.
    #[test]
    fn trailing_content_is_not_a_definition() {
        for line in [
            "[a]: /url \"title\" ok",
            "[a]: /url ok",
            "[a]:",
            "[a]: ",
            "[]: /u",
            "[a] /u",
            "a: /u",
            "[a]: /u(",
            // A footnote name is not empty and holds no whitespace, and without the colon this is
            // just text. `[^ a]: x` and `[^]: x` are left out: both are ordinary link reference
            // definitions, whose labels may hold spaces and a bare `^`.
            "[^a] x",
            // Four spaces of indentation is an indented code block, not a definition.
            "    [a]: /u",
            "    [^a]: x",
        ] {
            assert!(
                !is_definition_line(line),
                "should not be a definition: {line:?}"
            );
        }
    }

    /// micromark pairs bare `$` delimiters whatever sits around them, where comrak's `math_dollars`
    /// follows pandoc and needs a non-space inside each marker. Each shape below was measured against
    /// markdownlint-cli2 v0.23.3 by whether it reports a bare URL the span would swallow.
    #[test]
    fn inline_math_pairs_whatever_surrounds_the_dollars() {
        let spans = |source: &str| inline_math_spans(source, 0, source.len(), &[]);
        // Spaces either side of both markers, which comrak refuses to pair.
        assert_eq!(vec![(0, 5)], spans("$ a $"));
        assert_eq!(vec![(0, 5), (8, 13)], spans("$ a $ b $ c $"));
        // An escaped `$` is literal, so what is left is one unpaired delimiter.
        assert!(spans("\\$ a $").is_empty());
        // An odd count leaves the last one pairing with nothing.
        assert!(spans("$ a").is_empty());
        assert_eq!(1, spans("$ a $ b $").len());

        // A `$` inside a code span, or inside math comrak already found, delimits nothing.
        assert_eq!(
            vec![(5, 10)],
            inline_math_spans("`x$` $ a $", 0, 10, &[(0, 4)])
        );
        // Nothing outside the range asked about is considered.
        assert!(inline_math_spans("$ a $", 0, 1, &[]).is_empty());
    }

    /// micromark's `sequenceClose` compares the closing run's length with the opening one and, when
    /// they differ, re-marks the whole run as content and keeps looking. Pairing individual dollars
    /// instead — which is what a `chunks(2)` over the delimiter list does — splits `$a$$b$` into two
    /// spans that happen to cover the same bytes, and splits `$a$$$b$` into two that do not.
    #[test]
    fn inline_math_pairs_runs_not_individual_dollars() {
        let spans = |source: &str| inline_math_spans(source, 0, source.len(), &[]);
        // The run of two is content, so the single delimiters pair across it.
        assert_eq!(vec![(0, 6)], spans("$a$$b$"));
        assert_eq!(vec![(0, 7)], spans("$a$$$b$"));
        // A run of two needs a run of two to close it; the singles after it pair with each other.
        assert_eq!(vec![(3, 7)], spans("$$a$ b$"));
        assert_eq!(vec![(0, 5)], spans("$$a$$"));
        // Neither run is closed, so there is no math at all.
        assert!(spans("$a$$b").is_empty());
    }
}
