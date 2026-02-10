use anyhow::Result;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyles {
    pub base: Style,
    pub heading: [Style; 6],
    pub link_color: Color,
    pub inline_code: Style,
    pub prefix: Style,
    pub rule: Style,
    pub code_block_bg: Option<Color>,
    pub code_border: Style,
    pub code_header: Style,
    pub table_border: Style,
    pub table_header: Style,
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub level: u8,
    pub title: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
struct HeadingRaw {
    level: u8,
    title: String,
    raw_line: usize,
}

pub struct ParsedDocument {
    raw_lines: Vec<Line<'static>>,
    headings: Vec<HeadingRaw>,
}

pub struct RenderedDocument {
    pub lines: Vec<Line<'static>>,
    pub plain_lines: Vec<String>,
    pub headings: Vec<Heading>,
    pub matches: Vec<Match>,
}

pub fn parse_markdown(
    input: &str,
    syntax_set: &SyntaxSet,
    theme: &Theme,
    styles: &MarkdownStyles,
    tab_width: usize,
) -> Result<ParsedDocument> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let normalized = normalize_line_endings(input);
    let parser = Parser::new_ext(normalized.as_ref(), options);

    let mut raw_lines: Vec<Line<'static>> = Vec::new();
    let mut headings: Vec<HeadingRaw> = Vec::new();

    let mut line = LineBuilder::new();
    let mut heading: Option<HeadingBuilder> = None;
    let mut code_block: Option<CodeBlock> = None;
    let mut table: Option<TableBuilder> = None;
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut pending_list_prefix: Option<String> = None;
    let mut blockquote_level: usize = 0;

    let mut style_state = StyleState::new(styles.base, styles.link_color);

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if table.is_none() {
                        line.ensure_prefix(
                            &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                            styles.prefix,
                        );
                    }
                }
                Tag::Heading { level, .. } => {
                    flush_line(&mut line, &mut raw_lines);
                    heading = Some(HeadingBuilder::new(level as u8));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut line, &mut raw_lines);
                    code_block = Some(CodeBlock::new(kind));
                }
                Tag::Table(alignments) => {
                    flush_line(&mut line, &mut raw_lines);
                    table = Some(TableBuilder::new(alignments));
                }
                Tag::TableHead => {
                    if let Some(table) = table.as_mut() {
                        table.in_head = true;
                        table.saw_head = true;
                    }
                }
                Tag::TableRow => {
                    if let Some(table) = table.as_mut() {
                        table.start_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = table.as_mut() {
                        table.start_cell();
                    }
                }
                Tag::List(start) => list_stack.push(ListKind::from(start)),
                Tag::Item => {
                    flush_line(&mut line, &mut raw_lines);
                    pending_list_prefix = Some(list_prefix(&mut list_stack));
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                }
                Tag::Emphasis => style_state.italic += 1,
                Tag::Strong => style_state.bold += 1,
                Tag::Strikethrough => style_state.strike += 1,
                Tag::BlockQuote => {
                    blockquote_level += 1;
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                }
                Tag::Link { .. } => style_state.underline += 1,
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if table.is_none() {
                        flush_line(&mut line, &mut raw_lines);
                        push_blank_line(&mut raw_lines);
                    }
                }
                TagEnd::Heading(_) => {
                    if let Some(h) = heading.take() {
                        let text = h.text.trim().to_string();
                        let raw_line = raw_lines.len();
                        if h.level <= 2 && !raw_lines.is_empty() {
                            push_blank_line(&mut raw_lines);
                        }
                        raw_lines.push(Line::from(Span::styled(
                            text.clone(),
                            heading_style(styles, h.level),
                        )));
                        if h.level <= 2 {
                            let ch = if h.level == 1 { '═' } else { '─' };
                            let underline =
                                ch.to_string().repeat(text.chars().count().clamp(4, 48));
                            raw_lines.push(Line::from(Span::styled(underline, styles.rule)));
                        }
                        // plain lines are reconstructed later from spans
                        headings.push(HeadingRaw {
                            level: h.level,
                            title: text,
                            raw_line,
                        });
                        push_blank_line(&mut raw_lines);
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(block) = code_block.take() {
                        render_code_block(&block, syntax_set, theme, styles, &mut raw_lines);
                        push_blank_line(&mut raw_lines);
                    }
                }
                TagEnd::Table => {
                    if let Some(mut table_state) = table.take() {
                        table_state.end_row();
                        render_table(&table_state, styles, &mut raw_lines);
                        push_blank_line(&mut raw_lines);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = table.as_mut() {
                        // Be tolerant of parser event ordering and ensure header row is committed
                        // before we leave the head section.
                        table.end_row();
                        table.in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = table.as_mut() {
                        table.end_row();
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = table.as_mut() {
                        table.end_cell();
                    }
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    flush_line(&mut line, &mut raw_lines);
                }
                TagEnd::Item => {
                    pending_list_prefix = None;
                    flush_line(&mut line, &mut raw_lines);
                }
                TagEnd::Emphasis => style_state.italic = style_state.italic.saturating_sub(1),
                TagEnd::Strong => style_state.bold = style_state.bold.saturating_sub(1),
                TagEnd::Strikethrough => style_state.strike = style_state.strike.saturating_sub(1),
                TagEnd::BlockQuote => {
                    blockquote_level = blockquote_level.saturating_sub(1);
                    flush_line(&mut line, &mut raw_lines);
                    push_blank_line(&mut raw_lines);
                }
                TagEnd::Link => style_state.underline = style_state.underline.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(table) = table.as_mut() {
                    table.push_text(&text, style_state.inline_style(), tab_width);
                } else if let Some(h) = heading.as_mut() {
                    h.text.push_str(&text);
                } else if let Some(block) = code_block.as_mut() {
                    block.text.push_str(&text);
                } else {
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                    line.push_text(&text, style_state.current_style(), tab_width);
                }
            }
            Event::Code(text) => {
                if let Some(table) = table.as_mut() {
                    let inline = styles.inline_code.patch(style_state.inline_style());
                    table.push_text(&text, inline, tab_width);
                } else if let Some(h) = heading.as_mut() {
                    h.text.push_str(&text);
                } else if let Some(block) = code_block.as_mut() {
                    block.text.push_str(&text);
                } else {
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                    line.push_text(&text, styles.inline_code, tab_width);
                }
            }
            Event::SoftBreak => {
                if let Some(table) = table.as_mut() {
                    table.push_break(style_state.inline_style(), tab_width);
                } else if let Some(block) = code_block.as_mut() {
                    block.text.push('\n');
                } else {
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                    line.push_text(" ", style_state.current_style(), tab_width);
                }
            }
            Event::HardBreak => {
                if let Some(table) = table.as_mut() {
                    table.push_break(style_state.inline_style(), tab_width);
                } else {
                    flush_line(&mut line, &mut raw_lines);
                }
            }
            Event::Rule => {
                flush_line(&mut line, &mut raw_lines);
                raw_lines.push(Line::from(Span::styled("─".repeat(48), styles.rule)));
                push_blank_line(&mut raw_lines);
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                line.ensure_prefix(
                    &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                    styles.prefix,
                );
                line.push_text(marker, styles.prefix, tab_width);
            }
            _ => {}
        }
    }

    flush_line(&mut line, &mut raw_lines);

    Ok(ParsedDocument {
        raw_lines,
        headings,
    })
}

fn normalize_line_endings(input: &str) -> Cow<'_, str> {
    if input.contains('\r') {
        Cow::Owned(input.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(input)
    }
}

pub fn wrap_document(
    parsed: &ParsedDocument,
    width: u16,
    query: Option<&str>,
    case_sensitive: bool,
) -> RenderedDocument {
    let width = width.max(1);
    let mut wrapped_lines: Vec<Line<'static>> = Vec::new();
    let mut raw_to_wrapped: Vec<usize> = Vec::with_capacity(parsed.raw_lines.len());

    for line in &parsed.raw_lines {
        raw_to_wrapped.push(wrapped_lines.len());
        let mut wrapped = wrap_line(line, width as usize);
        if wrapped.is_empty() {
            wrapped.push(Line::from(""));
        }
        wrapped_lines.extend(wrapped);
    }

    let mut headings = Vec::new();
    for h in &parsed.headings {
        let line = raw_to_wrapped
            .get(h.raw_line)
            .copied()
            .unwrap_or(0);
        headings.push(Heading {
            level: h.level,
            title: h.title.clone(),
            line,
        });
    }

    let mut plain_lines: Vec<String> = wrapped_lines.iter().map(line_to_plain).collect();

    let matches = if let Some(q) = query {
        if q.is_empty() {
            Vec::new()
        } else {
            find_matches(&plain_lines, q, case_sensitive)
        }
    } else {
        Vec::new()
    };

    if !matches.is_empty() {
        let match_map = build_match_map(&matches);
        let highlighted: Vec<Line<'static>> = wrapped_lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                if let Some(ranges) = match_map.get(&idx) {
                    apply_highlight(line, ranges)
                } else {
                    line.clone()
                }
            })
            .collect();
        wrapped_lines = highlighted;
    }

    if !matches.is_empty() {
        plain_lines = wrapped_lines.iter().map(line_to_plain).collect();
    }

    RenderedDocument {
        lines: wrapped_lines,
        plain_lines,
        headings,
        matches,
    }
}

pub fn find_matches(lines: &[String], query: &str, case_sensitive: bool) -> Vec<Match> {
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_ascii_lowercase()
    };

    let mut out = Vec::new();
    for (line_idx, line) in lines.iter().enumerate() {
        if needle.is_empty() {
            break;
        }
        let hay = if case_sensitive {
            line.clone()
        } else {
            line.to_ascii_lowercase()
        };
        let mut cursor = 0;
        while cursor < hay.len() {
            if let Some(found) = hay[cursor..].find(&needle) {
                let start = cursor + found;
                let end = start + needle.len();
                out.push(Match {
                    line: line_idx,
                    start,
                    end,
                });
                cursor = end;
            } else {
                break;
            }
        }
    }
    out
}

fn heading_style(styles: &MarkdownStyles, level: u8) -> Style {
    let idx = level.saturating_sub(1).min(5) as usize;
    styles.heading[idx]
}

fn build_match_map(matches: &[Match]) -> std::collections::HashMap<usize, Vec<std::ops::Range<usize>>> {
    let mut map: std::collections::HashMap<usize, Vec<std::ops::Range<usize>>> =
        std::collections::HashMap::new();
    for m in matches {
        map.entry(m.line)
            .or_default()
            .push(m.start..m.end);
    }
    for ranges in map.values_mut() {
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for r in ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if r.start <= last.end {
                    last.end = last.end.max(r.end);
                    continue;
                }
            }
            merged.push(r);
        }
        *ranges = merged;
    }
    map
}

fn apply_highlight(line: &Line<'static>, ranges: &[std::ops::Range<usize>]) -> Line<'static> {
    if ranges.is_empty() {
        return line.clone();
    }

    let mut out_spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;

    for span in &line.spans {
        let text = span.content.as_ref();
        let span_start = cursor;
        let span_end = cursor + text.len();

        let mut local_idx = 0usize;
        for range in ranges.iter().filter(|r| r.end > span_start && r.start < span_end) {
            let start = range.start.max(span_start);
            let end = range.end.min(span_end);
            let local_start = start - span_start;
            let local_end = end - span_start;

            if local_start > local_idx {
                out_spans.push(Span::styled(
                    text[local_idx..local_start].to_string(),
                    span.style,
                ));
            }

            out_spans.push(Span::styled(
                text[local_start..local_end].to_string(),
                span.style.add_modifier(Modifier::REVERSED),
            ));

            local_idx = local_end;
        }

        if local_idx < text.len() {
            out_spans.push(Span::styled(
                text[local_idx..].to_string(),
                span.style,
            ));
        }

        cursor += text.len();
    }

    Line::from(out_spans)
}

fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }

    let fill_bg = line_uniform_bg(line);
    let fill_style = fill_bg.map(|bg| Style::default().bg(bg));
    let fill_width = if width > 500 { None } else { Some(width) };

    let tokens = tokenize_line(line);
    if tokens.is_empty() {
        if let (Some(style), Some(fill_width)) = (fill_style, fill_width) {
            return vec![Line::from(Span::styled(" ".repeat(fill_width), style))];
        }
        return vec![Line::from("")];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    let push_current = |current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        if current.is_empty() {
            if let (Some(style), Some(fill_width)) = (fill_style, fill_width) {
                out.push(Line::from(Span::styled(" ".repeat(fill_width), style)));
            } else {
                out.push(Line::from(""));
            }
            return;
        }
        trim_trailing_ws(current);
        if let (Some(style), Some(fill_width)) = (fill_style, fill_width) {
            let width_now = spans_width(current);
            if width_now < fill_width {
                current.push(Span::styled(" ".repeat(fill_width - width_now), style));
            }
        }
        out.push(Line::from(current.drain(..).collect::<Vec<_>>()));
    };

    for token in tokens {
        if token.is_whitespace {
            if current.is_empty() {
                continue;
            }
            let w = UnicodeWidthStr::width(token.text.as_str());
            if current_width + w > width {
                push_current(&mut current, &mut out);
                current_width = 0;
                continue;
            }
            current.push(Span::styled(token.text, token.style));
            current_width += w;
            continue;
        }

        let token_width = UnicodeWidthStr::width(token.text.as_str());
        if token_width <= width {
            if current_width + token_width > width && !current.is_empty() {
                push_current(&mut current, &mut out);
                current_width = 0;
            }
            current.push(Span::styled(token.text, token.style));
            current_width += token_width;
        } else {
            if !current.is_empty() {
                push_current(&mut current, &mut out);
                current_width = 0;
            }
            let mut buf = String::new();
            let mut buf_width = 0usize;
            for ch in token.text.chars() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if buf_width + ch_width > width && !buf.is_empty() {
                    out.push(Line::from(Span::styled(buf.clone(), token.style)));
                    buf.clear();
                    buf_width = 0;
                }
                buf.push(ch);
                buf_width += ch_width;
            }
            if !buf.is_empty() {
                current.push(Span::styled(buf, token.style));
                current_width = buf_width;
            }
        }
    }

    push_current(&mut current, &mut out);
    out
}

fn trim_trailing_ws(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(' ');
        if trimmed.len() == last.content.len() {
            break;
        }
        if trimmed.is_empty() {
            spans.pop();
            continue;
        }
        last.content = trimmed.to_string().into();
        break;
    }
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn line_uniform_bg(line: &Line<'static>) -> Option<Color> {
    let mut bg: Option<Color> = None;
    for span in &line.spans {
        if span.content.is_empty() {
            continue;
        }
        let Some(color) = span.style.bg else {
            return None;
        };
        if color == Color::Reset {
            return None;
        }
        match bg {
            Some(existing) if existing != color => return None,
            Some(_) => {}
            None => bg = Some(color),
        }
    }
    bg
}

fn tokenize_line(line: &Line<'static>) -> Vec<Token> {
    let mut tokens = Vec::new();
    for span in &line.spans {
        let text = span.content.as_ref();
        if text.is_empty() {
            continue;
        }
        let mut buf = String::new();
        let mut current_ws: Option<bool> = None;
        for ch in text.chars() {
            let is_ws = ch.is_whitespace();
            if current_ws.is_none() {
                current_ws = Some(is_ws);
            }
            if Some(is_ws) != current_ws {
                tokens.push(Token {
                    text: buf.clone(),
                    style: span.style,
                    is_whitespace: current_ws.unwrap_or(false),
                });
                buf.clear();
                current_ws = Some(is_ws);
            }
            buf.push(ch);
        }
        if !buf.is_empty() {
            tokens.push(Token {
                text: buf,
                style: span.style,
                is_whitespace: current_ws.unwrap_or(false),
            });
        }
    }
    tokens
}

fn line_to_plain(line: &Line<'static>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        out.push_str(span.content.as_ref());
    }
    out
}

fn render_table(table: &TableBuilder, styles: &MarkdownStyles, raw_lines: &mut Vec<Line<'static>>) {
    if table.rows.is_empty() {
        return;
    }
    let column_count = table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0);
    if column_count == 0 {
        return;
    }

    let mut widths = vec![0usize; column_count];
    for row in &table.rows {
        for (idx, cell) in row.cells.iter().enumerate() {
            let width = UnicodeWidthStr::width(cell.text.as_str());
            widths[idx] = widths[idx].max(width);
        }
    }

    let border = styles.table_border;
    raw_lines.push(table_border_line(
        &widths,
        ('┌', '┬', '┐'),
        border,
    ));

    let mut header_len = 0usize;
    for row in &table.rows {
        if row.is_header {
            header_len += 1;
        } else {
            break;
        }
    }
    if header_len == 0 && table.saw_head && !table.rows.is_empty() {
        // Defensive fallback: if a header section existed but row flags were lost,
        // treat the first row as header to preserve expected table structure.
        header_len = 1;
    }

    for row in table.rows.iter().take(header_len) {
        raw_lines.push(table_row_line(
            row,
            &widths,
            &table.alignments,
            styles.table_header,
            border,
        ));
    }
    if header_len > 0 && header_len < table.rows.len() {
        raw_lines.push(table_border_line(
            &widths,
            ('├', '┼', '┤'),
            border,
        ));
    }
    for row in table.rows.iter().skip(header_len) {
        raw_lines.push(table_row_line(
            row,
            &widths,
            &table.alignments,
            styles.base,
            border,
        ));
    }

    raw_lines.push(table_border_line(
        &widths,
        ('└', '┴', '┘'),
        border,
    ));
}

fn table_border_line(
    widths: &[usize],
    joints: (char, char, char),
    style: Style,
) -> Line<'static> {
    let mut line = String::new();
    line.push(joints.0);
    for (idx, width) in widths.iter().enumerate() {
        line.push_str(&"─".repeat(width.saturating_add(2)));
        if idx + 1 < widths.len() {
            line.push(joints.1);
        }
    }
    line.push(joints.2);
    Line::from(Span::styled(line, style))
}

fn table_row_line(
    row: &TableRow,
    widths: &[usize],
    alignments: &[Alignment],
    cell_style: Style,
    border_style: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    spans.push(Span::styled("│", border_style));
    for idx in 0..widths.len() {
        let cell = row.cells.get(idx);
        let text = cell.map(|c| c.text.as_str()).unwrap_or("");
        let align = alignments
            .get(idx)
            .copied()
            .unwrap_or(Alignment::Left);
        let text_width = UnicodeWidthStr::width(text);
        let (left_pad, right_pad) = cell_padding(text_width, widths[idx], align);

        spans.push(Span::styled(" ".repeat(1 + left_pad), cell_style));
        if let Some(cell) = cell {
            for fragment in &cell.spans {
                spans.push(Span::styled(
                    fragment.text.clone(),
                    cell_style.patch(fragment.style),
                ));
            }
        }
        spans.push(Span::styled(" ".repeat(1 + right_pad), cell_style));
        spans.push(Span::styled("│", border_style));
    }
    Line::from(spans)
}

fn cell_padding(text_width: usize, width: usize, align: Alignment) -> (usize, usize) {
    if width <= text_width {
        return (0, 0);
    }
    let pad = width - text_width;
    match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            (left, right)
        }
        _ => (0, pad),
    }
}

fn render_code_block(
    block: &CodeBlock,
    syntax_set: &SyntaxSet,
    theme: &Theme,
    styles: &MarkdownStyles,
    raw_lines: &mut Vec<Line<'static>>,
) {
    let syntax = resolve_code_syntax(syntax_set, block.language.as_deref());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let code_bg = styles.code_block_bg;
    let border_style = styles.code_border;
    let header_style = styles.code_header;
    let pad_style = Style::default().bg(code_bg.unwrap_or(Color::Reset));

    let mut max_width = 0usize;
    for line in LinesWithEndings::from(&block.text) {
        let text = line.trim_end_matches('\n');
        let width = UnicodeWidthStr::width(text);
        if width > max_width {
            max_width = width;
        }
    }
    let inner_width = max_width.saturating_add(2);

    let label = block
        .language
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("code");
    let header = format!(" {label} ");
    let header_width = UnicodeWidthStr::width(header.as_str());
    if header_width + 2 <= inner_width {
        let dashes = inner_width - header_width;
        let left = dashes / 2;
        let right = dashes - left;
        raw_lines.push(Line::from(vec![
            Span::styled("┌", border_style),
            Span::styled("─".repeat(left), border_style),
            Span::styled(header, header_style),
            Span::styled("─".repeat(right), border_style),
            Span::styled("┐", border_style),
        ]));
    } else {
        raw_lines.push(Line::from(Span::styled(
            format!("┌{}┐", "─".repeat(inner_width)),
            border_style,
        )));
    }
    raw_lines.push(Line::from(vec![
        Span::styled("│", border_style),
        Span::styled(" ".repeat(inner_width), pad_style),
        Span::styled("│", border_style),
    ]));

    for line in LinesWithEndings::from(&block.text) {
        let ranges = match highlighter.highlight_line(line, syntax_set) {
            Ok(r) => r,
            Err(_) => vec![(syntect::highlighting::Style::default(), line)],
        };
        let mut spans = vec![
            Span::styled("│", border_style),
            Span::styled(" ", pad_style),
        ];
        let mut line_width = 0usize;
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            line_width += UnicodeWidthStr::width(text);
            spans.push(Span::styled(
                text.to_string(),
                syntect_to_ratatui(style, code_bg),
            ));
        }
        if line_width < max_width {
            spans.push(Span::styled(" ".repeat(max_width - line_width), pad_style));
        }
        spans.push(Span::styled(" ", pad_style));
        spans.push(Span::styled("│", border_style));
        raw_lines.push(Line::from(spans));
    }

    raw_lines.push(Line::from(vec![
        Span::styled("│", border_style),
        Span::styled(" ".repeat(inner_width), pad_style),
        Span::styled("│", border_style),
    ]));
    let bottom = format!("└{}┘", "─".repeat(inner_width));
    raw_lines.push(Line::from(Span::styled(bottom, border_style)));
}

fn resolve_code_syntax<'a>(
    syntax_set: &'a SyntaxSet,
    lang: Option<&str>,
) -> &'a syntect::parsing::SyntaxReference {
    let Some(lang) = lang.map(|l| l.trim()).filter(|l| !l.is_empty()) else {
        return syntax_set.find_syntax_plain_text();
    };
    let token = lang.strip_prefix("language-").unwrap_or(lang);
    let candidates = language_candidates(token);
    for cand in candidates {
        if let Some(syntax) = syntax_set.find_syntax_by_token(&cand) {
            return syntax;
        }
        if let Some(syntax) = syntax_set.find_syntax_by_extension(&cand) {
            return syntax;
        }
    }
    syntax_set.find_syntax_plain_text()
}

fn language_candidates(lang: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = lang.to_ascii_lowercase();
    match lower.as_str() {
        "elixir" | "ex" | "exs" => {
            out.push("Elixir".to_string());
            out.push("elixir".to_string());
            out.push("ex".to_string());
            out.push("exs".to_string());
        }
        _ => {}
    }
    out.push(lang.to_string());
    out
}

fn syntect_to_ratatui(style: syntect::highlighting::Style, code_bg: Option<Color>) -> Style {
    let mut out = Style::default()
        .fg(Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b));
    if let Some(bg) = code_bg {
        out = out.bg(bg);
    } else if style.background.a > 0 {
        out = out.bg(Color::Rgb(
            style.background.r,
            style.background.g,
            style.background.b,
        ));
    }
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

struct StyleState {
    base: Style,
    link_color: Color,
    bold: u8,
    italic: u8,
    strike: u8,
    underline: u8,
}

impl StyleState {
    fn new(base: Style, link_color: Color) -> Self {
        Self {
            base,
            link_color,
            bold: 0,
            italic: 0,
            strike: 0,
            underline: 0,
        }
    }

    fn current_style(&self) -> Style {
        self.base.patch(self.inline_style())
    }

    fn inline_style(&self) -> Style {
        let mut style = Style::default();
        if self.underline > 0 {
            style = style
                .fg(self.link_color)
                .add_modifier(Modifier::UNDERLINED);
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }
}

struct LineBuilder {
    spans: Vec<Span<'static>>,
    plain: String,
}

impl LineBuilder {
    fn new() -> Self {
        Self {
            spans: Vec::new(),
            plain: String::new(),
        }
    }

    fn ensure_prefix(&mut self, prefix: &str, style: Style) {
        if self.plain.is_empty() && !prefix.is_empty() {
            self.spans.push(Span::styled(prefix.to_string(), style));
            self.plain.push_str(prefix);
        }
    }

    fn push_text(&mut self, text: &str, style: Style, tab_width: usize) {
        let expanded = expand_tabs(text, tab_width);
        self.spans.push(Span::styled(expanded.clone(), style));
        self.plain.push_str(&expanded);
    }

    fn take_line(&mut self) -> Option<(Line<'static>, String)> {
        if self.plain.is_empty() {
            return None;
        }
        let line = Line::from(self.spans.drain(..).collect::<Vec<_>>());
        let plain = std::mem::take(&mut self.plain);
        Some((line, plain))
    }
}

fn flush_line(builder: &mut LineBuilder, raw_lines: &mut Vec<Line<'static>>) {
    if let Some((line, plain)) = builder.take_line() {
        raw_lines.push(line);
        let _ = plain;
    }
}

fn push_blank_line(raw_lines: &mut Vec<Line<'static>>) {
    raw_lines.push(Line::from(""));
}

fn list_prefix(stack: &mut [ListKind]) -> String {
    let depth = stack.len().max(1);
    let indent = "  ".repeat(depth.saturating_sub(1));
    let prefix = match stack.last_mut() {
        Some(ListKind::Bullet) => format!("{} ", bullet_for_depth(depth)),
        Some(ListKind::Ordered { next }) => {
            let current = *next;
            *next = next.saturating_add(1);
            format!("{current}. ")
        }
        None => "- ".to_string(),
    };
    format!("{indent}{prefix}")
}

fn current_prefix(blockquote_level: usize, list_prefix: Option<&str>) -> String {
    let mut out = String::new();
    if blockquote_level > 0 {
        out.push_str(&"│ ".repeat(blockquote_level));
    }
    if let Some(prefix) = list_prefix {
        out.push_str(prefix);
    }
    out
}

fn expand_tabs(text: &str, tab_width: usize) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let spaces = " ".repeat(tab_width.max(1));
    text.replace('\t', &spaces)
}

struct HeadingBuilder {
    level: u8,
    text: String,
}

impl HeadingBuilder {
    fn new(level: u8) -> Self {
        Self {
            level,
            text: String::new(),
        }
    }
}

struct CodeBlock {
    language: Option<String>,
    text: String,
}

impl CodeBlock {
    fn new(kind: CodeBlockKind) -> Self {
        let language = match kind {
            CodeBlockKind::Fenced(lang) => {
                let trimmed = lang.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            CodeBlockKind::Indented => None,
        };
        Self {
            language,
            text: String::new(),
        }
    }
}

#[derive(Clone)]
struct TableSpan {
    text: String,
    style: Style,
}

#[derive(Clone)]
struct TableCell {
    text: String,
    spans: Vec<TableSpan>,
}

#[derive(Clone)]
struct TableRow {
    cells: Vec<TableCell>,
    is_header: bool,
}

struct TableBuilder {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Vec<TableCell>,
    current_cell: String,
    current_cell_spans: Vec<TableSpan>,
    in_head: bool,
    saw_head: bool,
}

impl TableBuilder {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
            current_cell_spans: Vec::new(),
            in_head: false,
            saw_head: false,
        }
    }

    fn start_row(&mut self) {
        self.current_row.clear();
        self.current_cell.clear();
        self.current_cell_spans.clear();
    }

    fn start_cell(&mut self) {
        self.current_cell.clear();
        self.current_cell_spans.clear();
    }

    fn push_text(&mut self, text: &str, style: Style, tab_width: usize) {
        let expanded = expand_tabs(text, tab_width);
        if expanded.is_empty() {
            return;
        }
        self.current_cell.push_str(&expanded);
        if let Some(last) = self.current_cell_spans.last_mut() {
            if last.style == style {
                last.text.push_str(&expanded);
                return;
            }
        }
        self.current_cell_spans.push(TableSpan {
            text: expanded,
            style,
        });
    }

    fn push_break(&mut self, style: Style, tab_width: usize) {
        if !self.current_cell.ends_with(' ') {
            self.push_text(" ", style, tab_width);
        }
    }

    fn end_cell(&mut self) {
        let cell = self.current_cell.trim().to_string();
        let spans = trim_table_spans(&self.current_cell_spans);
        self.current_row.push(TableCell { text: cell, spans });
        self.current_cell.clear();
        self.current_cell_spans.clear();
    }

    fn end_row(&mut self) {
        if !self.current_cell.is_empty() {
            self.end_cell();
        }
        if self.current_row.is_empty() {
            return;
        }
        let row = TableRow {
            cells: self.current_row.clone(),
            is_header: self.in_head,
        };
        self.rows.push(row);
        self.current_row.clear();
    }
}

#[derive(Clone)]
struct Token {
    text: String,
    style: Style,
    is_whitespace: bool,
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered { next: u64 },
}

impl ListKind {
    fn from(start: Option<u64>) -> Self {
        match start {
            Some(num) => Self::Ordered { next: num },
            None => Self::Bullet,
        }
    }
}

fn bullet_for_depth(depth: usize) -> &'static str {
    match depth % 3 {
        1 => "•",
        2 => "◦",
        _ => "▪",
    }
}

fn trim_table_spans(spans: &[TableSpan]) -> Vec<TableSpan> {
    let mut styled_chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            styled_chars.push((ch, span.style));
        }
    }
    if styled_chars.is_empty() {
        return Vec::new();
    }

    let Some(start) = styled_chars.iter().position(|(ch, _)| !ch.is_whitespace()) else {
        return Vec::new();
    };
    let end = styled_chars
        .iter()
        .rposition(|(ch, _)| !ch.is_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    if start >= end {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current_style = styled_chars[start].1;
    let mut current_text = String::new();

    for (ch, style) in styled_chars[start..end].iter().copied() {
        if style != current_style {
            if !current_text.is_empty() {
                out.push(TableSpan {
                    text: std::mem::take(&mut current_text),
                    style: current_style,
                });
            }
            current_style = style;
        }
        current_text.push(ch);
    }

    if !current_text.is_empty() {
        out.push(TableSpan {
            text: current_text,
            style: current_style,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_line_endings, render_table, wrap_line, MarkdownStyles, TableBuilder, TableCell,
        TableRow, TableSpan,
    };
    use pulldown_cmark::Alignment;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::widgets::Widget;
    use std::borrow::Cow;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;

    #[test]
    fn normalize_line_endings_preserves_lf_input() {
        let input = "a\nb\n";
        let normalized = normalize_line_endings(input);
        assert!(matches!(normalized, Cow::Borrowed(_)));
        assert_eq!(normalized.as_ref(), input);
    }

    #[test]
    fn normalize_line_endings_converts_crlf_and_cr() {
        let input = "a\r\nb\rc\r\n";
        let normalized = normalize_line_endings(input);
        assert_eq!(normalized.as_ref(), "a\nb\nc\n");
    }

    #[test]
    fn render_table_respects_detected_header_even_without_row_flag() {
        let mut table = TableBuilder::new(vec![Alignment::Left, Alignment::Left]);
        table.saw_head = true;
        table.rows.push(TableRow {
            cells: vec![
                table_cell("Key", Style::default()),
                table_cell("Action", Style::default()),
            ],
            is_header: false,
        });
        table.rows.push(TableRow {
            cells: vec![
                table_cell("a", Style::default()),
                table_cell("Add", Style::default()),
            ],
            is_header: false,
        });

        let styles = test_styles();
        let mut lines = Vec::new();
        render_table(&table, &styles, &mut lines);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|l| l.contains(" Key ")));
        assert!(rendered.iter().any(|l| l.starts_with('├')));
    }

    #[test]
    fn render_table_applies_bold_to_header_cells() {
        let mut table = TableBuilder::new(vec![Alignment::Left, Alignment::Left]);
        table.rows.push(TableRow {
            cells: vec![
                table_cell("Key", Style::default()),
                table_cell("Action", Style::default()),
            ],
            is_header: true,
        });
        table.rows.push(TableRow {
            cells: vec![
                table_cell("a", Style::default()),
                table_cell("Add", Style::default()),
            ],
            is_header: false,
        });

        let mut styles = test_styles();
        styles.table_header = Style::default().add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        render_table(&table, &styles, &mut lines);

        let header_line = &lines[1];
        assert!(header_line.spans.iter().any(|span| {
            span.content.contains("Key")
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn render_table_applies_inline_bold_to_body_cell() {
        let mut table = TableBuilder::new(vec![Alignment::Left, Alignment::Left]);
        table.rows.push(TableRow {
            cells: vec![
                table_cell("Key", Style::default()),
                table_cell("Action", Style::default()),
            ],
            is_header: true,
        });
        table.rows.push(TableRow {
            cells: vec![
                table_cell("File Operations", Style::default().add_modifier(Modifier::BOLD)),
                table_cell("", Style::default()),
            ],
            is_header: false,
        });

        let styles = test_styles();
        let mut lines = Vec::new();
        render_table(&table, &styles, &mut lines);

        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains("File Operations")
                    && span.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
    }

    #[test]
    fn wrap_line_preserves_bold_modifier() {
        let line = Line::from(vec![Span::styled(
            " Key Action ",
            Style::default().add_modifier(Modifier::BOLD),
        )]);
        let wrapped = wrap_line(&line, 4);
        assert!(!wrapped.is_empty());
        assert!(wrapped.iter().any(|wrapped_line| {
            wrapped_line
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn paragraph_render_keeps_bold_modifier() {
        let text = ratatui::text::Text::from(vec![Line::from(vec![Span::styled(
            "Header",
            Style::default().add_modifier(Modifier::BOLD),
        )])]);
        let paragraph = Paragraph::new(text).style(Style::default().fg(Color::White));
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        paragraph.render(area, &mut buf);

        assert!((0..6).any(|x| buf.get(x, 0).style().add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn parse_markdown_preserves_bold_in_table_cell() {
        let markdown = "\
| Key | Action |
| --- | --- |
| **File Operations** | Add |
";
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .expect("default syntect theme");
        let styles = test_styles();

        let parsed = super::parse_markdown(markdown, &syntax_set, theme, &styles, 4)
            .expect("parse should succeed");
        let bold_found = parsed.raw_lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains("File Operations")
                    && span.style.add_modifier.contains(Modifier::BOLD)
            })
        });
        assert!(bold_found);
    }

    fn table_cell(text: &str, style: Style) -> TableCell {
        TableCell {
            text: text.to_string(),
            spans: if text.is_empty() {
                Vec::new()
            } else {
                vec![TableSpan {
                    text: text.to_string(),
                    style,
                }]
            },
        }
    }

    fn test_styles() -> MarkdownStyles {
        MarkdownStyles {
            base: Style::default().fg(Color::White),
            heading: [Style::default(); 6],
            link_color: Color::Blue,
            inline_code: Style::default(),
            prefix: Style::default(),
            rule: Style::default(),
            code_block_bg: None,
            code_border: Style::default(),
            code_header: Style::default(),
            table_border: Style::default(),
            table_header: Style::default(),
        }
    }
}
