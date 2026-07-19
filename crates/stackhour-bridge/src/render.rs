//! Output shaping for Telegram, split from telegram.rs.
//!
//! Every function here is a byte-for-byte port of the corresponding helper in
//! the Node coordinator (`/home/nikita/.claude-remote/coordinator.mjs`) or
//! `tg-send.mjs`. Where the JS measures a string it measures UTF-16 code
//! units, so the ports do the same: chunk boundaries and table column widths
//! must land on the same offsets or the rendered output diverges the moment a
//! message contains an emoji.
//!
//! * [`esc`] — the ONLY escaper: `&`, `<`, `>`, in that order. Quotes are not
//!   escaped (coordinator.mjs:173).
//! * [`render_html`] — fenced code -> `<pre>`, inline code -> `<code>`.
//!   Escaping happens BEFORE the inline-code pass and the capture is
//!   re-inserted unescaped; that order is load-bearing (coordinator.mjs:174).
//! * [`has_table`] / [`rewrite_tables`] — markdown-table detection and the
//!   padded-code-block rewrite (coordinator.mjs:182-198).
//! * [`chunk_text`] — the newline-preferring chunker shared by deliverFinal
//!   (3800) and tg-send (4000), including the 50% hard-cut floor. The split
//!   newline stays at the HEAD of the next chunk.
//! * [`html_chunks`] — [`chunk_text`] followed by [`render_html`], which is
//!   exactly what deliverFinal's fallback does.
//! * [`fmt_dur`], [`claude_activity`], [`codex_activity`].

use serde_json::Value;
use std::sync::OnceLock;

/// deliverFinal's fallback chunk limit (coordinator.mjs:204).
pub const DELIVER_FINAL_CHUNK_LIMIT: usize = 3800;
/// tg-send's chunk limit — deliberately a DIFFERENT number for the same job
/// (tg-send.mjs:57).
pub const TG_SEND_CHUNK_LIMIT: usize = 4000;
/// Activity-line detail truncation width (coordinator.mjs:215).
pub const ACTIVITY_DETAIL_WIDTH: usize = 80;
/// The fraction of the limit below which a newline split point is abandoned
/// for a hard cut.
const HARD_CUT_FLOOR: f64 = 0.5;
/// Column gutter in a rewritten table.
const TABLE_GUTTER: &str = "  ";

/// HTML-escape, exactly as `esc()` does: `&` then `<` then `>`, nothing else.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Number of UTF-16 code units, i.e. what JS `String.prototype.length`
/// reports. Used everywhere the JS measures a string.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn inline_code_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"`([^`\n]+)`").expect("static regex"))
}

fn info_line_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_+-]*$").expect("static regex"))
}

fn sep_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^\s*\|?[\s:|-]*-{1,}[\s:|-]*\|?\s*$").expect("static regex")
    })
}

/// Markdown -> Telegram HTML, byte-for-byte with `renderHtml()`.
///
/// The text is split on ``` ``` ```; odd parts are fenced code (the info line
/// is dropped only when it is entirely `[a-zA-Z0-9_+-]`), even parts are prose
/// (escaped first, then inline backticks become `<code>`).
pub fn render_html(text: &str) -> String {
    let mut out = String::new();
    for (i, part) in text.split("```").enumerate() {
        if i % 2 == 1 {
            out.push_str("<pre>");
            out.push_str(&esc(&strip_info_line(part)));
            out.push_str("</pre>");
        } else {
            let escaped = esc(part);
            let replaced = inline_code_re().replace_all(&escaped, |c: &regex::Captures| {
                format!("<code>{}</code>", &c[1])
            });
            out.push_str(&replaced);
        }
    }
    out
}

/// `.replace(/^[^\n]*\n/, m => /^[a-zA-Z0-9_+-]*\s*$/.test(m.trim()) ? '' : m)`
fn strip_info_line(code: &str) -> String {
    let Some(nl) = code.find('\n') else {
        return code.to_string();
    };
    let matched = &code[..=nl];
    if info_line_re().is_match(matched.trim()) {
        code[nl + 1..].to_string()
    } else {
        code.to_string()
    }
}

/// `isSep()`: a markdown table separator row. Both conditions, in order.
pub fn is_sep(line: &str) -> bool {
    line.contains('-') && sep_re().is_match(line)
}

/// `splitRow()`: trim, strip ONE leading and ONE trailing pipe, split, trim
/// each cell. No `\|` escape handling — the JS has none either.
pub fn split_row(line: &str) -> Vec<String> {
    let mut s = line.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    s.split('|').map(|c| c.trim().to_string()).collect()
}

/// `hasTable()`: any line containing a pipe whose successor is a separator.
pub fn has_table(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    for i in 0..lines.len().saturating_sub(1) {
        if lines[i].contains('|') && is_sep(lines[i + 1]) {
            return true;
        }
    }
    false
}

/// `asciiTables()`: rewrite every markdown table into a padded fenced block.
///
/// Column widths are UTF-16 code-unit counts, matching JS `.length`. That is
/// already wrong for emoji and wide glyphs in the Node — reproducing it is the
/// point, because the owner's existing output is the contract.
pub fn rewrite_tables(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].contains('|') && i + 1 < lines.len() && is_sep(lines[i + 1]) {
            let header = split_row(lines[i]);
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut j = i + 2;
            while j < lines.len() && lines[j].contains('|') && !is_sep(lines[j]) {
                rows.push(split_row(lines[j]));
                j += 1;
            }
            let cols = header
                .len()
                .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
                .max(1);
            let mut w = vec![0usize; cols];
            for r in std::iter::once(&header).chain(rows.iter()) {
                for (c, width) in w.iter_mut().enumerate() {
                    let cell = r.get(c).map(String::as_str).unwrap_or("");
                    *width = (*width).max(utf16_len(cell));
                }
            }
            let fmt = |r: &Vec<String>| -> String {
                let cells: Vec<String> = (0..cols)
                    .map(|c| {
                        let cell = r.get(c).map(String::as_str).unwrap_or("");
                        let pad = w[c].saturating_sub(utf16_len(cell));
                        format!("{cell}{}", " ".repeat(pad))
                    })
                    .collect();
                cells.join(TABLE_GUTTER).trim_end().to_string()
            };
            let mut block: Vec<String> = Vec::with_capacity(rows.len() + 2);
            block.push(fmt(&header));
            block.push(
                w.iter()
                    .map(|x| "-".repeat((*x).max(1)))
                    .collect::<Vec<String>>()
                    .join(TABLE_GUTTER),
            );
            block.extend(rows.iter().map(fmt));
            out.push(format!("```\n{}\n```", block.join("\n")));
            i = j;
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    out.join("\n")
}

/// The shared chunker: split at the last newline at or before `limit`, unless
/// that lands in the first half, in which case cut hard at `limit`.
///
/// The newline is NOT consumed — it leads the next chunk, so continuation
/// chunks begin with a blank line. That is observable in the owner's chat
/// today and is preserved deliberately.
///
/// Always returns at least one element, even for an empty input.
///
/// The one place this cannot match the reference exactly: when a hard cut
/// lands in the MIDDLE of a surrogate pair (a limit-sized run of emoji with no
/// newline), JS produces two chunks each holding half of the pair, while Rust
/// has no way to represent a lone surrogate in a `String` and substitutes
/// U+FFFD. The split OFFSET is identical either way, so chunk boundaries and
/// chunk counts still agree; only that one broken glyph differs, and it was
/// already broken in the Node.
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    if limit == 0 {
        return vec![text.to_string()];
    }
    while units.len() - start > limit {
        // JS: s.lastIndexOf('\n', limit) over the REMAINING string.
        let mut cut: i64 = -1;
        for offset in (0..=limit).rev() {
            if units[start + offset] == b'\n' as u16 {
                cut = offset as i64;
                break;
            }
        }
        if (cut as f64) < limit as f64 * HARD_CUT_FLOOR {
            cut = limit as i64;
        }
        let cut = cut as usize;
        out.push(String::from_utf16_lossy(&units[start..start + cut]));
        start += cut;
    }
    out.push(String::from_utf16_lossy(&units[start..]));
    out
}

/// deliverFinal's fallback body: chunk the RAW text, then render each chunk as
/// Telegram HTML.
///
/// Note the known Node bug this reproduces: the limit is applied to the
/// PRE-render text, but `render_html` expands `&` to `&amp;` and adds tags, so
/// a chunk dense in those characters can render past Telegram's 4096-char cap
/// and be rejected with a 400 — silently losing that chunk. Measuring the
/// rendered length would be the fix, but it would move every chunk boundary,
/// so it is left alone and flagged instead.
pub fn html_chunks(text: &str, limit: usize) -> Vec<String> {
    chunk_text(text, limit).iter().map(|c| render_html(c)).collect()
}

/// `fmtDur()`: seconds under a minute, `<m>m<s>s` above. No hour unit, so 95
/// minutes renders as `95m12s`.
pub fn fmt_dur(ms: i64) -> String {
    let s = (ms as f64 / 1000.0).round() as i64;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{}s", s / 60, s % 60)
    }
}

/// `String(d).replace(/\s+/g, ' ').slice(0, 80)` — whitespace collapsed, then
/// truncated at 80 UTF-16 code units.
fn truncate_detail(detail: &str) -> String {
    let mut collapsed = String::with_capacity(detail.len());
    let mut in_ws = false;
    for ch in detail.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                collapsed.push(' ');
                in_ws = true;
            }
        } else {
            collapsed.push(ch);
            in_ws = false;
        }
    }
    let units: Vec<u16> = collapsed.encode_utf16().collect();
    if units.len() <= ACTIVITY_DETAIL_WIDTH {
        collapsed
    } else {
        String::from_utf16_lossy(&units[..ACTIVITY_DETAIL_WIDTH])
    }
}

/// `activityLine(tool)` for a claude `tool_use` block. `None` where the JS
/// returns `''`.
pub fn claude_activity(tool: &Value) -> Option<String> {
    let name = tool.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let input = tool.get("input");
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    let detail = if name == "Bash" {
        field("command").unwrap_or_default()
    } else {
        field("file_path")
            .or_else(|| field("pattern"))
            .or_else(|| field("url"))
            .or_else(|| field("command"))
            .unwrap_or_default()
    };
    let detail = truncate_detail(detail);
    Some(if detail.is_empty() {
        format!("⚙️ {name}")
    } else {
        format!("⚙️ {name}: {detail}")
    })
}

/// `codexActivity(item)` for a codex JSONL item. `None` where the JS returns
/// `''` (a falsy item or an `agent_message`).
pub fn codex_activity(item: &Value) -> Option<String> {
    let ty = item.get("type")?.as_str()?;
    if ty.is_empty() || ty == "agent_message" {
        return None;
    }
    let field = |k: &str| item.get(k).and_then(Value::as_str);
    let detail = field("command")
        .or_else(|| field("query"))
        .or_else(|| field("name"))
        .or_else(|| field("path"))
        .unwrap_or_default();
    let detail = truncate_detail(detail);
    let label = match ty {
        "command_execution" => "Command",
        "file_change" => "File change",
        "mcp_tool_call" => "Tool",
        "web_search" => "Web search",
        "todo_list" => "Plan",
        other => other,
    };
    Some(if detail.is_empty() {
        format!("⚙️ {label}")
    } else {
        format!("⚙️ {label}: {detail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- esc ----

    #[test]
    fn esc_escapes_exactly_three_entities_and_leaves_quotes_alone() {
        assert_eq!(esc(r#"a & b < c > d " e ' f"#), "a &amp; b &lt; c &gt; d \" e ' f");
    }

    #[test]
    fn esc_orders_the_ampersand_first_so_entities_are_not_double_escaped() {
        // If '<' were replaced before '&', this would come out as '&amp;lt;'.
        assert_eq!(esc("<"), "&lt;");
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    // ---- render_html ----

    #[test]
    fn render_html_wraps_a_fence_in_pre_and_drops_the_language_line() {
        assert_eq!(
            render_html("before\n```rust\nlet x = 1 < 2;\n```\nafter"),
            "before\n<pre>let x = 1 &lt; 2;\n</pre>\nafter"
        );
    }

    #[test]
    fn render_html_keeps_a_first_line_that_is_not_a_bare_language_tag() {
        // 'not a lang' contains spaces, so it is CONTENT, not an info string.
        assert_eq!(
            render_html("```\nnot a lang\ncode\n```"),
            "<pre>not a lang\ncode\n</pre>"
        );
    }

    #[test]
    fn render_html_escapes_before_inlining_code_spans() {
        // The capture is re-inserted unescaped because it was already escaped.
        assert_eq!(render_html("use `a<b` here"), "use <code>a&lt;b</code> here");
    }

    #[test]
    fn render_html_leaves_an_unmatched_backtick_literal() {
        assert_eq!(render_html("a ` b"), "a ` b");
    }

    #[test]
    fn render_html_treats_an_odd_fence_count_as_trailing_prose() {
        assert_eq!(render_html("a\n```\nb"), "a\n<pre>b</pre>");
    }

    // ---- tables ----

    #[test]
    fn is_sep_needs_both_a_dash_and_the_shape() {
        assert!(is_sep("|---|---|"));
        assert!(is_sep(" :--- | ---: "));
        assert!(!is_sep("| a | b |"));
        assert!(!is_sep("|   |   |")); // no dash
    }

    #[test]
    fn split_row_strips_one_pipe_each_side_and_trims_cells() {
        assert_eq!(split_row("| a | b |"), vec!["a", "b"]);
        assert_eq!(split_row("a|b"), vec!["a", "b"]);
        assert_eq!(split_row("|| a ||"), vec!["", "a", ""]);
    }

    #[test]
    fn has_table_finds_a_header_followed_by_a_separator() {
        assert!(has_table("x\n| a | b |\n|---|---|\n| 1 | 2 |"));
        assert!(!has_table("| a | b |\n| 1 | 2 |"));
    }

    #[test]
    fn rewrite_tables_pads_columns_and_strips_trailing_space_per_row() {
        let out = rewrite_tables("| name | id |\n|---|---|\n| alpha | 1 |");
        assert_eq!(out, "```\nname   id\n-----  --\nalpha  1\n```");
        // The separator row keeps its full width; data rows do not.
        assert!(out.lines().any(|l| l == "-----  --"));
    }

    #[test]
    fn rewrite_tables_widens_every_row_when_a_row_has_extra_cells() {
        let out = rewrite_tables("| a |\n|---|\n| 1 | 2 |");
        assert_eq!(out, "```\na\n-  -\n1  2\n```");
    }

    #[test]
    fn rewrite_tables_leaves_non_table_lines_verbatim() {
        assert_eq!(rewrite_tables("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn rewrite_tables_measures_width_in_utf16_units_like_the_node() {
        // A non-BMP emoji is 2 UTF-16 units, so the Node pads it as width 2.
        let out = rewrite_tables("| 🚀 | b |\n|---|---|\n| xy | c |");
        assert_eq!(out, "```\n🚀  b\n--  -\nxy  c\n```");
    }

    // ---- chunking ----

    #[test]
    fn chunk_text_returns_the_whole_string_when_it_fits() {
        assert_eq!(chunk_text("short", 100), vec!["short"]);
        assert_eq!(chunk_text("", 100), vec![""]);
    }

    #[test]
    fn chunk_text_splits_at_the_last_newline_and_keeps_it_leading() {
        let text = format!("{}\n{}", "a".repeat(8), "b".repeat(8));
        let parts = chunk_text(&text, 10);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "a".repeat(8));
        assert_eq!(parts[1], format!("\n{}", "b".repeat(8)));
    }

    #[test]
    fn chunk_text_hard_cuts_when_the_newline_lands_in_the_first_half() {
        // Newline at index 2 of a limit-10 window: 2 < 5, so cut at 10.
        let text = format!("ab\n{}", "c".repeat(20));
        let parts = chunk_text(&text, 10);
        assert_eq!(parts[0].chars().count(), 10);
        assert_eq!(parts[0], "ab\nccccccc");
    }

    #[test]
    fn chunk_text_cuts_at_the_same_utf16_offset_even_mid_surrogate_pair() {
        // Verified against the Node by differential test: the boundaries and
        // the chunk count agree; only the half-pair glyph differs, because
        // Rust cannot hold a lone surrogate.
        let parts = chunk_text(&"🚀".repeat(10), 7);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].encode_utf16().count(), 7);
        assert_eq!(parts[1].encode_utf16().count(), 7);
        assert_eq!(parts[2].encode_utf16().count(), 6);
    }

    #[test]
    fn chunk_text_reassembles_to_the_original() {
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let joined: String = chunk_text(&text, 3800).concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn html_chunks_renders_every_chunk() {
        let out = html_chunks("a < b", DELIVER_FINAL_CHUNK_LIMIT);
        assert_eq!(out, vec!["a &lt; b"]);
    }

    // ---- fmt_dur ----

    #[test]
    fn fmt_dur_rounds_to_seconds_and_has_no_hour_unit() {
        assert_eq!(fmt_dur(0), "0s");
        assert_eq!(fmt_dur(1500), "2s"); // Math.round, not floor
        assert_eq!(fmt_dur(59_400), "59s");
        assert_eq!(fmt_dur(60_000), "1m0s");
        assert_eq!(fmt_dur(90_000), "1m30s");
        assert_eq!(fmt_dur(5_712_000), "95m12s");
    }

    // ---- activity lines ----

    #[test]
    fn claude_activity_prefers_the_command_for_bash() {
        assert_eq!(
            claude_activity(&json!({"name": "Bash", "input": {"command": "ls  -l\n", "file_path": "/x"}})),
            Some("⚙️ Bash: ls -l ".to_string())
        );
    }

    #[test]
    fn claude_activity_walks_the_field_precedence_for_other_tools() {
        assert_eq!(
            claude_activity(&json!({"name": "Read", "input": {"file_path": "/a", "pattern": "p"}})),
            Some("⚙️ Read: /a".to_string())
        );
        assert_eq!(
            claude_activity(&json!({"name": "Grep", "input": {"pattern": "p"}})),
            Some("⚙️ Grep: p".to_string())
        );
        assert_eq!(
            claude_activity(&json!({"name": "Task"})),
            Some("⚙️ Task".to_string())
        );
    }

    #[test]
    fn claude_activity_truncates_the_detail_at_80_units() {
        let line = claude_activity(&json!({"name": "Bash", "input": {"command": "x".repeat(200)}}))
            .expect("line");
        assert_eq!(line, format!("⚙️ Bash: {}", "x".repeat(80)));
    }

    #[test]
    fn codex_activity_maps_known_types_and_skips_agent_messages() {
        assert_eq!(codex_activity(&json!({"type": "agent_message", "text": "hi"})), None);
        assert_eq!(
            codex_activity(&json!({"type": "command_execution", "command": "cargo test"})),
            Some("⚙️ Command: cargo test".to_string())
        );
        assert_eq!(
            codex_activity(&json!({"type": "web_search", "query": "rust"})),
            Some("⚙️ Web search: rust".to_string())
        );
        assert_eq!(
            codex_activity(&json!({"type": "something_new"})),
            Some("⚙️ something_new".to_string())
        );
    }
}
