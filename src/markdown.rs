use anyhow::Result;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyles {
    pub base: Style,
    pub heading: Style,
    pub link_color: Color,
    pub inline_code: Style,
    pub prefix: Style,
    pub rule: Style,
    pub code_bg: Option<Color>,
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

    let parser = Parser::new_ext(input, options);

    let mut raw_lines: Vec<Line<'static>> = Vec::new();
    let mut headings: Vec<HeadingRaw> = Vec::new();

    let mut line = LineBuilder::new();
    let mut heading: Option<HeadingBuilder> = None;
    let mut code_block: Option<CodeBlock> = None;
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut pending_list_prefix: Option<String> = None;
    let mut blockquote_level: usize = 0;

    let mut style_state = StyleState::new(styles.base, styles.link_color);

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    line.ensure_prefix(
                        &current_prefix(blockquote_level, pending_list_prefix.as_deref()),
                        styles.prefix,
                    );
                }
                Tag::Heading { level, .. } => {
                    flush_line(&mut line, &mut raw_lines);
                    heading = Some(HeadingBuilder::new(level as u8));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut line, &mut raw_lines);
                    code_block = Some(CodeBlock::new(kind));
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
                    flush_line(&mut line, &mut raw_lines);
                    push_blank_line(&mut raw_lines);
                }
                TagEnd::Heading(_) => {
                    if let Some(h) = heading.take() {
                        let text = h.text.trim().to_string();
                        let raw_line = raw_lines.len();
                        raw_lines.push(Line::from(Span::styled(text.clone(), styles.heading)));
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
                        render_code_block(&block, syntax_set, theme, styles.code_bg, &mut raw_lines);
                        push_blank_line(&mut raw_lines);
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
                if let Some(h) = heading.as_mut() {
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
                if let Some(h) = heading.as_mut() {
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
                if let Some(block) = code_block.as_mut() {
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
                flush_line(&mut line, &mut raw_lines);
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

    let tokens = tokenize_line(line);
    if tokens.is_empty() {
        return vec![Line::from("")];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    let push_current = |current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        if current.is_empty() {
            out.push(Line::from(""));
        } else {
            trim_trailing_ws(current);
            out.push(Line::from(current.drain(..).collect::<Vec<_>>()));
        }
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

fn render_code_block(
    block: &CodeBlock,
    syntax_set: &SyntaxSet,
    theme: &Theme,
    code_bg: Option<Color>,
    raw_lines: &mut Vec<Line<'static>>,
) {
    let syntax = resolve_code_syntax(syntax_set, block.language.as_deref());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let gutter_style = Style::default().bg(code_bg.unwrap_or(Color::Reset));
    for line in LinesWithEndings::from(&block.text) {
        let ranges = match highlighter.highlight_line(line, syntax_set) {
            Ok(r) => r,
            Err(_) => vec![(syntect::highlighting::Style::default(), line)],
        };
        let mut spans = vec![Span::styled("  ", gutter_style)];
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                text.to_string(),
                syntect_to_ratatui(style, code_bg),
            ));
        }
        if spans.len() == 1 {
            spans.push(Span::styled(" ", gutter_style));
        }
        raw_lines.push(Line::from(spans));
    }
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
        let mut style = self.base;
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
    let indent = "  ".repeat(stack.len().saturating_sub(1));
    let prefix = stack.last_mut().map(ListKind::prefix).unwrap_or("- ".to_string());
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

    fn prefix(&mut self) -> String {
        match self {
            Self::Bullet => "- ".to_string(),
            Self::Ordered { next } => {
                let current = *next;
                *next = next.saturating_add(1);
                format!("{current}. ")
            }
        }
    }
}
