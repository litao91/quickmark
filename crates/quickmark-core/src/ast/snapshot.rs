//! Checked-in renderings of the tree, so a comrak bump is reviewable by reading a diff.
//!
//! An oracle that diffed the tree against tree-sitter-md node-for-node is what made the move to
//! comrak safe: it ran over thirteen hand-written structural cases, the 354 migrated markdownlint
//! fixtures and all 2747 files of a real vault, and every one was byte-exact. These snapshots are
//! what is left after it. They are weaker on purpose — thirteen documents rather than three
//! thousand — but they need no second parser, and a change to any node kind, position or child order
//! shows up as a line in a diff a human can read.
//!
//! Each rendering is `(kind start_row:start_col-end_row:end_col start_byte-end_byte`, indented one
//! level per parent. Two conventions make the numbers look odd and are the point of pinning them:
//! columns count UTF-8 bytes, and a block's end includes its trailing newline, so a one-line
//! paragraph on row 0 ends at `1:0`.

use super::build;
use super::Node;

fn render(source: &str) -> String {
    let tree = build::parse(source);
    let mut out = String::new();
    push(tree.root_node(), 0, &mut out);
    out
}

fn push(node: Node, depth: usize, out: &mut String) {
    let start = node.start_position();
    let end = node.end_position();
    let indent = "  ".repeat(depth);
    out.push_str(&format!(
        "{indent}({} {}:{}-{}:{} {}-{}",
        node.kind(),
        start.row,
        start.column,
        end.row,
        end.column,
        node.start_byte(),
        node.end_byte()
    ));
    if node.child_count() == 0 {
        out.push_str(")\n");
        return;
    }
    out.push('\n');
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            push(child, depth + 1, out);
        }
    }
    out.push_str(&indent);
    out.push_str(")\n");
}

/// One case per synthesis family. Sources are short on purpose: the rendering is meant to be read.
const CASES: &[(&str, &str, &str)] = &[
    (
        "atx headings",
        "# One\n\n##  Two  ##\n\n###\n",
        r#"(document 0:0-5:0 0-24
  (section 0:0-5:0 0-24
    (atx_heading 0:0-1:0 0-6
      (atx_h1_marker 0:0-0:1 0-1)
      (inline 0:2-0:5 2-5
        (text 0:2-0:5 2-5)
      )
    )
    (section 2:0-5:0 7-24
      (atx_heading 2:0-3:0 7-19
        (atx_h2_marker 2:0-2:2 7-9)
        (inline 2:4-2:11 11-18
          (text 2:4-2:7 11-14)
        )
      )
      (section 4:0-5:0 20-24
        (atx_heading 4:0-5:0 20-24
          (atx_h3_marker 4:0-4:3 20-23)
        )
      )
    )
  )
)
"#,
    ),
    (
        "setext headings",
        "Title\n=====\n\nSub\n---\n",
        r#"(document 0:0-5:0 0-21
  (section 0:0-5:0 0-21
    (setext_heading 0:0-2:0 0-12
      (paragraph 0:0-1:0 0-6
        (inline 0:0-0:5 0-5
          (text 0:0-0:5 0-5)
        )
      )
      (setext_h1_underline 1:0-1:5 6-11)
    )
    (setext_heading 3:0-5:0 13-21
      (paragraph 3:0-4:0 13-17
        (inline 3:0-3:3 13-16
          (text 3:0-3:3 13-16)
        )
      )
      (setext_h2_underline 4:0-4:3 17-20)
    )
  )
)
"#,
    ),
    (
        "front matter",
        "---\ntitle: x\n---\n\n# H\n\n+++\nother: y\n+++\n",
        r#"(document 0:0-9:0 0-40
  (minus_metadata 0:0-3:0 0-17)
  (section 3:0-4:0 17-18)
  (section 4:0-9:0 18-40
    (atx_heading 4:0-5:0 18-22
      (atx_h1_marker 4:0-4:1 18-19)
      (inline 4:2-4:3 20-21
        (text 4:2-4:3 20-21)
      )
    )
    (paragraph 6:0-9:0 23-40
      (inline 6:0-8:3 23-39
        (text 6:0-6:3 23-26)
        (text 7:0-7:8 27-35)
        (text 8:0-8:3 36-39)
      )
    )
  )
)
"#,
    ),
    (
        "front matter plus",
        "+++\ntitle: x\n+++\n\n# H\n",
        r#"(document 0:0-5:0 0-22
  (plus_metadata 0:0-3:0 0-17)
  (section 3:0-4:0 17-18)
  (section 4:0-5:0 18-22
    (atx_heading 4:0-5:0 18-22
      (atx_h1_marker 4:0-4:1 18-19)
      (inline 4:2-4:3 20-21
        (text 4:2-4:3 20-21)
      )
    )
  )
)
"#,
    ),
    (
        "lists",
        "- tight\n- items\n\n1. loose\n\n2. items\n\n- [x] task\n",
        r#"(document 0:0-8:0 0-48
  (section 0:0-8:0 0-48
    (list 0:0-3:0 0-17
      (list_item 0:0-1:0 0-8
        (list_marker_minus 0:0-0:2 0-2)
        (paragraph 0:2-1:0 2-8
          (inline 0:2-0:7 2-7
            (text 0:2-0:7 2-7)
          )
        )
      )
      (list_item 1:0-3:0 8-17
        (list_marker_minus 1:0-1:2 8-10)
        (paragraph 1:2-2:0 10-16
          (inline 1:2-1:7 10-15
            (text 1:2-1:7 10-15)
          )
        )
      )
    )
    (list 3:0-7:0 17-37
      (list_item 3:0-5:0 17-27
        (list_marker_dot 3:0-3:3 17-20)
        (paragraph 3:3-4:0 20-26
          (inline 3:3-3:8 20-25
            (text 3:3-3:8 20-25)
          )
        )
      )
      (list_item 5:0-7:0 27-37
        (list_marker_dot 5:0-5:3 27-30)
        (paragraph 5:3-6:0 30-36
          (inline 5:3-5:8 30-35
            (text 5:3-5:8 30-35)
          )
        )
      )
    )
    (list 7:0-8:0 37-48
      (list_item 7:0-8:0 37-48
        (list_marker_minus 7:0-7:2 37-39)
        (paragraph 7:6-8:0 43-48
          (inline 7:6-7:10 43-47
            (text 7:6-7:10 43-47)
          )
        )
      )
    )
  )
)
"#,
    ),
    (
        "nested list",
        "- outer\n  - inner\n    - deep\n",
        r#"(document 0:0-3:0 0-29
  (section 0:0-3:0 0-29
    (list 0:0-3:0 0-29
      (list_item 0:0-3:0 0-29
        (list_marker_minus 0:0-0:2 0-2)
        (paragraph 0:2-1:2 2-10
          (inline 0:2-0:7 2-7
            (text 0:2-0:7 2-7)
          )
        )
        (list 1:2-3:0 10-29
          (list_item 1:2-3:0 10-29
            (list_marker_minus 1:2-1:4 10-12)
            (paragraph 1:4-2:4 12-22
              (inline 1:4-1:9 12-17
                (text 1:4-1:9 12-17)
              )
            )
            (list 2:4-3:0 22-29
              (list_item 2:4-3:0 22-29
                (list_marker_minus 2:4-2:6 22-24)
                (paragraph 2:6-3:0 24-29
                  (inline 2:6-2:10 24-28
                    (text 2:6-2:10 24-28)
                  )
                )
              )
            )
          )
        )
      )
    )
  )
)
"#,
    ),
    (
        "fenced code",
        "```rust\nlet x = 1;\n```\n\n~~~\nunclosed\n",
        r#"(document 0:0-6:0 0-37
  (section 0:0-6:0 0-37
    (fenced_code_block 0:0-3:0 0-23
      (code_fence_content 1:0-2:0 8-19)
    )
    (fenced_code_block 4:0-6:0 24-37
      (code_fence_content 5:0-6:0 28-37)
    )
  )
)
"#,
    ),
    (
        "indented code",
        "para\n\n    code\n    more\n\ntext\n",
        r#"(document 0:0-6:0 0-30
  (section 0:0-6:0 0-30
    (paragraph 0:0-1:0 0-5
      (inline 0:0-0:4 0-4
        (text 0:0-0:4 0-4)
      )
    )
    (indented_code_block 2:0-5:0 6-25)
    (paragraph 5:0-6:0 25-30
      (inline 5:0-5:4 25-29
        (text 5:0-5:4 25-29)
      )
    )
  )
)
"#,
    ),
    (
        "block quote",
        "> quoted\n> more\n>\n> - a list\n",
        r#"(document 0:0-4:0 0-29
  (section 0:0-4:0 0-29
    (block_quote 0:0-4:0 0-29
      (paragraph 0:2-2:1 2-17
        (inline 0:2-1:6 2-15
          (text 0:2-0:8 2-8)
          (text 1:2-1:6 11-15)
        )
      )
      (list 3:2-4:0 20-29
        (list_item 3:2-4:0 20-29
          (list_marker_minus 3:2-3:4 20-22)
          (paragraph 3:4-4:0 22-29
            (inline 3:4-3:10 22-28
              (text 3:4-3:10 22-28)
            )
          )
        )
      )
    )
  )
)
"#,
    ),
    (
        "code in a block quote",
        ">     code\n\n>     more\n\n> para\n",
        r#"(document 0:0-5:0 0-31
  (section 0:0-5:0 0-31
    (block_quote 0:0-1:0 0-11
      (indented_code_block 0:2-1:0 2-11)
    )
    (block_quote 2:0-3:0 12-23
      (indented_code_block 2:2-3:0 14-23)
    )
    (block_quote 4:0-5:0 24-31
      (paragraph 4:2-5:0 26-31
        (inline 4:2-4:6 26-30
          (text 4:2-4:6 26-30)
        )
      )
    )
  )
)
"#,
    ),
    (
        "table",
        "| a | b |\n|:--|--:|\n| 1 |\n",
        r#"(document 0:0-3:0 0-26
  (section 0:0-3:0 0-26
    (pipe_table 0:0-3:0 0-26
      (pipe_table_header 0:0-0:9 0-9
        (| 0:0-0:1 0-1)
        (pipe_table_cell 0:2-0:4 2-4
          (inline 0:2-0:3 2-3
            (text 0:2-0:3 2-3)
          )
        )
        (| 0:4-0:5 4-5)
        (pipe_table_cell 0:6-0:8 6-8
          (inline 0:6-0:7 6-7
            (text 0:6-0:7 6-7)
          )
        )
        (| 0:8-0:9 8-9)
      )
      (pipe_table_delimiter_row 1:0-1:9 10-19
        (| 1:0-1:1 10-11)
        (pipe_table_delimiter_cell 1:1-1:4 11-14)
        (| 1:4-1:5 14-15)
        (pipe_table_delimiter_cell 1:5-1:8 15-18)
        (| 1:8-1:9 18-19)
      )
      (pipe_table_row 2:0-2:5 20-25
        (| 2:0-2:1 20-21)
        (pipe_table_cell 2:2-2:4 22-24
          (inline 2:2-2:3 22-23
            (text 2:2-2:3 22-23)
          )
        )
        (| 2:4-2:5 24-25)
      )
    )
  )
)
"#,
    ),
    (
        "reference definitions",
        "[a]: /one\n[b]: /two \"t\"\n\nUse [a].\n",
        r#"(document 0:0-4:0 0-34
  (section 0:0-4:0 0-34
    (link_reference_definition 0:0-1:0 0-10)
    (link_reference_definition 1:0-2:0 10-24)
    (paragraph 3:0-4:0 25-34
      (inline 3:0-3:8 25-33
        (text 3:0-3:4 25-29)
        (link 3:4-3:7 29-32
          (text 3:5-3:6 30-31)
        )
        (text 3:7-3:8 32-33)
      )
    )
  )
)
"#,
    ),
    (
        "html and thematic break",
        "<div>\n  x\n</div>\n\n***\n",
        r#"(document 0:0-5:0 0-22
  (section 0:0-5:0 0-22
    (html_block 0:0-3:0 0-17)
    (thematic_break 4:0-5:0 18-22)
  )
)
"#,
    ),
    // Every kind the builder can emit under `inline`, including nesting, an escaped character and a
    // character reference. The `text` spans are raw source, so `a\*b` keeps its backslash and `&amp;`
    // keeps its five bytes — comrak's decoded payload is not what a rule reads through `utf8_text`.
    (
        "inline kinds",
        "p *em* **st** ***b*** `c` [l](/u \"t\") ![i](/v) <b>h</b> a\\*b &amp;\n",
        r#"(document 0:0-1:0 0-67
  (section 0:0-1:0 0-67
    (paragraph 0:0-1:0 0-67
      (inline 0:0-0:66 0-66
        (text 0:0-0:2 0-2)
        (emphasis 0:2-0:6 2-6
          (text 0:3-0:5 3-5)
        )
        (text 0:6-0:7 6-7)
        (strong_emphasis 0:7-0:13 7-13
          (text 0:9-0:11 9-11)
        )
        (text 0:13-0:14 13-14)
        (emphasis 0:14-0:21 14-21
          (strong_emphasis 0:15-0:20 15-20
            (text 0:17-0:18 17-18)
          )
        )
        (text 0:21-0:22 21-22)
        (code_span 0:22-0:25 22-25)
        (text 0:25-0:26 25-26)
        (link 0:26-0:37 26-37
          (text 0:27-0:28 27-28)
        )
        (text 0:37-0:38 37-38)
        (image 0:38-0:46 38-46
          (text 0:40-0:41 40-41)
        )
        (text 0:46-0:47 46-47)
        (html_inline 0:47-0:50 47-50)
        (text 0:50-0:51 50-51)
        (html_inline 0:51-0:55 51-55)
        (text 0:55-0:66 55-66)
      )
    )
  )
)
"#,
    ),
    // An emphasis spanning a line break, and a shortcut reference link that comrak resolved.
    (
        "inline across lines",
        "one *em\nstill em* three [ref] tail\n\n[ref]: /u\n",
        r#"(document 0:0-4:0 0-46
  (section 0:0-4:0 0-46
    (paragraph 0:0-2:0 0-35
      (inline 0:0-1:26 0-34
        (text 0:0-0:4 0-4)
        (emphasis 0:4-1:9 4-17
          (text 0:5-0:7 5-7)
          (text 1:0-1:8 8-16)
        )
        (text 1:9-1:16 17-24)
        (link 1:16-1:21 24-29
          (text 1:17-1:20 25-28)
        )
        (text 1:21-1:26 29-34)
      )
    )
    (link_reference_definition 3:0-4:0 36-46)
  )
)
"#,
    ),
    // A `$$…$$` region swallows the blocks comrak built inside it, because markdownlint's micromark
    // folds the whole thing into one `mathFlow` token and no Rust parser here does.
    (
        "math block",
        "# H\n\n$$\n# not a heading\n$$\n\ntext\n",
        r#"(document 0:0-7:0 0-33
  (section 0:0-7:0 0-33
    (atx_heading 0:0-1:0 0-4
      (atx_h1_marker 0:0-0:1 0-1)
      (inline 0:2-0:3 2-3
        (text 0:2-0:3 2-3)
      )
    )
    (math_block 2:0-5:0 5-27)
    (paragraph 6:0-7:0 28-33
      (inline 6:0-6:4 28-32
        (text 6:0-6:4 28-32)
      )
    )
  )
)
"#,
    ),
    (
        "inline $$ is not a block, an unclosed one runs on",
        "$$ x $$\n# B\n\n$$\nnever closed\n",
        r#"(document 0:0-5:0 0-29
  (section 0:0-1:0 0-8
    (paragraph 0:0-1:0 0-8
      (inline 0:0-0:7 0-7
        (math 0:0-0:7 0-7)
      )
    )
  )
  (section 1:0-5:0 8-29
    (atx_heading 1:0-2:0 8-12
      (atx_h1_marker 1:0-1:1 8-9)
      (inline 1:2-1:3 10-11
        (text 1:2-1:3 10-11)
      )
    )
    (math_block 3:0-5:0 13-29)
  )
)
"#,
    ),
    (
        "math block in a list item",
        "- a\n- $$\n  x\n  $$\n- b\n",
        r#"(document 0:0-5:0 0-22
  (section 0:0-5:0 0-22
    (list 0:0-5:0 0-22
      (list_item 0:0-1:0 0-4
        (list_marker_minus 0:0-0:2 0-2)
        (paragraph 0:2-1:0 2-4
          (inline 0:2-0:3 2-3
            (text 0:2-0:3 2-3)
          )
        )
      )
      (list_item 1:0-4:0 4-18
        (list_marker_minus 1:0-1:2 4-6)
        (math_block 1:2-4:0 6-18)
      )
      (list_item 4:0-5:0 18-22
        (list_marker_minus 4:0-4:2 18-20)
        (paragraph 4:2-5:0 20-22
          (inline 4:2-4:3 20-21
            (text 4:2-4:3 20-21)
          )
        )
      )
    )
  )
)
"#,
    ),
    (
        "no trailing newline",
        "# H\n\ntext",
        r#"(document 0:0-2:4 0-9
  (section 0:0-2:4 0-9
    (atx_heading 0:0-1:0 0-4
      (atx_h1_marker 0:0-0:1 0-1)
      (inline 0:2-0:3 2-3
        (text 0:2-0:3 2-3)
      )
    )
    (paragraph 2:0-2:4 5-9
      (inline 2:0-2:4 5-9
        (text 2:0-2:4 5-9)
      )
    )
  )
)
"#,
    ),
    (
        "table cell inline",
        "| *em* | `code` |\n|---|---|\n| [l](/u) | <b>h</b> |\n",
        r#"(document 0:0-3:0 0-51
  (section 0:0-3:0 0-51
    (pipe_table 0:0-3:0 0-51
      (pipe_table_header 0:0-0:17 0-17
        (| 0:0-0:1 0-1)
        (pipe_table_cell 0:2-0:7 2-7
          (inline 0:2-0:6 2-6
            (emphasis 0:2-0:6 2-6
              (text 0:3-0:5 3-5)
            )
          )
        )
        (| 0:7-0:8 7-8)
        (pipe_table_cell 0:9-0:16 9-16
          (inline 0:9-0:15 9-15
            (code_span 0:9-0:15 9-15)
          )
        )
        (| 0:16-0:17 16-17)
      )
      (pipe_table_delimiter_row 1:0-1:9 18-27
        (| 1:0-1:1 18-19)
        (pipe_table_delimiter_cell 1:1-1:4 19-22)
        (| 1:4-1:5 22-23)
        (pipe_table_delimiter_cell 1:5-1:8 23-26)
        (| 1:8-1:9 26-27)
      )
      (pipe_table_row 2:0-2:22 28-50
        (| 2:0-2:1 28-29)
        (pipe_table_cell 2:2-2:10 30-38
          (inline 2:2-2:9 30-37
            (link 2:2-2:9 30-37
              (text 2:3-2:4 31-32)
            )
          )
        )
        (| 2:10-2:11 38-39)
        (pipe_table_cell 2:12-2:21 40-49
          (inline 2:12-2:20 40-48
            (html_inline 2:12-2:15 40-43)
            (text 2:15-2:16 43-44)
            (html_inline 2:16-2:20 44-48)
          )
        )
        (| 2:21-2:22 49-50)
      )
    )
  )
)
"#,
    ),
    (
        "table short row",
        "| a | b |\n|---|---|\n| *x* |\n",
        r#"(document 0:0-3:0 0-28
  (section 0:0-3:0 0-28
    (pipe_table 0:0-3:0 0-28
      (pipe_table_header 0:0-0:9 0-9
        (| 0:0-0:1 0-1)
        (pipe_table_cell 0:2-0:4 2-4
          (inline 0:2-0:3 2-3
            (text 0:2-0:3 2-3)
          )
        )
        (| 0:4-0:5 4-5)
        (pipe_table_cell 0:6-0:8 6-8
          (inline 0:6-0:7 6-7
            (text 0:6-0:7 6-7)
          )
        )
        (| 0:8-0:9 8-9)
      )
      (pipe_table_delimiter_row 1:0-1:9 10-19
        (| 1:0-1:1 10-11)
        (pipe_table_delimiter_cell 1:1-1:4 11-14)
        (| 1:4-1:5 14-15)
        (pipe_table_delimiter_cell 1:5-1:8 15-18)
        (| 1:8-1:9 18-19)
      )
      (pipe_table_row 2:0-2:7 20-27
        (| 2:0-2:1 20-21)
        (pipe_table_cell 2:2-2:6 22-26
          (inline 2:2-2:5 22-25
            (emphasis 2:2-2:5 22-25
              (text 2:3-2:4 23-24)
            )
          )
        )
        (| 2:6-2:7 26-27)
      )
    )
  )
)
"#,
    ),
    (
        "inline math",
        "text $a_b$ and $$x^*$$ then *em*\n",
        r#"(document 0:0-1:0 0-33
  (section 0:0-1:0 0-33
    (paragraph 0:0-1:0 0-33
      (inline 0:0-0:32 0-32
        (text 0:0-0:5 0-5)
        (math 0:5-0:10 5-10)
        (text 0:10-0:15 10-15)
        (math 0:15-0:22 15-22)
        (text 0:22-0:28 22-28)
        (emphasis 0:28-0:32 28-32
          (text 0:29-0:31 29-31)
        )
      )
    )
  )
)
"#,
    ),
    (
        "math region clipped to its list item",
        "1. a\n\n   $$\n   x\n\n# H\n",
        r#"(document 0:0-6:0 0-22
  (section 0:0-5:0 0-18
    (list 0:0-5:0 0-18
      (list_item 0:0-5:0 0-18
        (list_marker_dot 0:0-0:3 0-3)
        (paragraph 0:3-1:0 3-5
          (inline 0:3-0:4 3-4
            (text 0:3-0:4 3-4)
          )
        )
        (math_block 2:3-4:0 9-17)
      )
    )
  )
  (section 5:0-6:0 18-22
    (atx_heading 5:0-6:0 18-22
      (atx_h1_marker 5:0-5:1 18-19)
      (inline 5:2-5:3 20-21
        (text 5:2-5:3 20-21)
      )
    )
  )
)
"#,
    ),
];

#[test]
fn snapshots() {
    let mut failures = Vec::new();
    for &(label, source, expected) in CASES {
        let actual = render(source);
        if actual != expected {
            failures.push(format!("--- {label} --- {source:?}\n{actual}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} tree snapshots changed. If the new trees are right, replace the expected \
         rendering; if not, this is a regression.\n\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n")
    );
}

#[test]
fn every_case_has_a_rendering() {
    let missing: Vec<&str> = CASES
        .iter()
        .filter(|&&(_, _, expected)| expected.is_empty())
        .map(|&(label, _, _)| label)
        .collect();
    assert!(
        missing.is_empty(),
        "these cases have no checked-in rendering yet: {missing:?}"
    );
}
