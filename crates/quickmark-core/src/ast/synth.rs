//! The pieces of a tree-sitter-md tree that comrak does not produce, re-derived from raw source.
//!
//! Every span here was measured against tree-sitter-md 0.5.3 rather than inferred from its grammar,
//! and `oracle.rs` is what keeps them honest. Two of comrak's behaviours make this necessary rather
//! than merely convenient: it detaches paragraphs that turn out to be pure link reference
//! definitions, and it autocompletes table cells to the header width while dropping the delimiter
//! row — so neither can be read off its tree.

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

    /// Whether `source` contains a `\r` that is not part of a `\r\n` pair. tree-sitter-md does not
    /// treat those as line breaks while comrak and CommonMark do, so the two parses of such a
    /// document cannot agree and the oracle skips them.
    #[cfg(test)]
    pub fn has_bare_carriage_return(source: &str) -> bool {
        let bytes = source.as_bytes();
        bytes
            .iter()
            .enumerate()
            .any(|(index, &byte)| byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
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

    /// Whether `row` carries any of this span's content. A span ending at column 0 stops before its
    /// end row begins.
    pub fn contains_row(self, row: u32) -> bool {
        let last = if self.end.1 == 0 {
            self.end.0.saturating_sub(1)
        } else {
            self.end.0
        };
        self.start.0 <= row && row <= last
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
    let content_end = lines.inline_end_col(row as usize);

    AtxParts {
        marker: Marker {
            kind: atx_marker_kind((hashes_end - hashes_start).clamp(1, 6) as u8),
            span: Span::new((row, start_col), (row, hashes_end)),
        },
        inline: (content_start < content_end)
            .then_some(Span::new((row, content_start), (row, content_end))),
    }
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

/// One line of a link reference definition: up to three spaces of indent, a label, a colon, a
/// destination, and then either the end of the line or a title and the end of the line.
///
/// That last rule is why this is a parser and not a `[label]:` pattern. Prose where only a title may
/// go makes the whole thing an ordinary paragraph, so `[^version]: It's generally good practice ...`
/// is not a definition — and inventing one adds a spurious unused-definition report for every
/// footnote in the document.
fn is_definition_line(text: &str) -> bool {
    let bytes = text.as_bytes();
    let indent = bytes.iter().take_while(|&&byte| byte == b' ').count();
    if indent > 3 || bytes.get(indent) != Some(&b'[') {
        return false;
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
    use super::is_definition_line;

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
        ] {
            assert!(is_definition_line(line), "should be a definition: {line:?}");
        }
    }

    /// Prose where only a title may go makes the line an ordinary paragraph. Getting this wrong
    /// invents a definition, which costs a spurious MD052/MD053 report for every footnote in a
    /// document — the `[^version]: It's generally good practice ...` shape is common in posts
    /// migrated from Jekyll.
    #[test]
    fn trailing_content_is_not_a_definition() {
        for line in [
            "[^version]: It's generally good practice for Rust posts",
            "[a]: /url \"title\" ok",
            "[a]: /url ok",
            "[a]:",
            "[a]: ",
            "[]: /u",
            "[a] /u",
            "a: /u",
            "[a]: /u(",
            // Four spaces of indentation is an indented code block, not a definition.
            "    [a]: /u",
        ] {
            assert!(
                !is_definition_line(line),
                "should not be a definition: {line:?}"
            );
        }
    }
}
