use crate::config::{self, Config};
use crate::markdown::{
    parse_markdown, wrap_document, Heading, MarkdownStyles, ParsedDocument, RenderedDocument,
};
use crate::theme::{ThemeManager, UiPalette};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, ExecutableCommand};
use notify::{RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use ropey::Rope;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthChar;

pub fn run_app(path: PathBuf, mut config: Config) -> Result<()> {
    let theme_manager = ThemeManager::load(&config)?;
    if !theme_manager.theme_names().iter().any(|t| t == &config.theme) {
        config.theme = theme_manager.fallback_name().to_string();
        config::write_config(&config)?;
    }

    let mut app = App::new(path, config, theme_manager)?;

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&app.file_path, RecursiveMode::NonRecursive)?;

    let tick_rate = Duration::from_millis(50);

    loop {
        let size = terminal.size()?;
        let layout = app.layout(size);
        let render_width = layout.preview_width.unwrap_or(layout.editor_width);
        let render_height = layout.preview_height.unwrap_or(layout.editor_height);
        app.ensure_rendered(render_width);
        app.sync_render_from_rope();
        app.clamp_scroll(render_height);
        if app.show_preview {
            app.update_render_cursor_line();
            app.ensure_rendered_cursor_visible(render_height);
        }

        terminal.draw(|f| ui(f, &mut app, &layout))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key, layout.editor_height) {
                    break;
                }
            }
        }

        while let Ok(msg) = rx.try_recv() {
            if let Ok(event) = msg {
                app.on_fs_event(event);
            }
        }

        app.handle_pending_reload();
    }

    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.execute(LeaveAlternateScreen);
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

#[derive(Default)]
struct FsReload {
    pending: bool,
    deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    SearchInput,
    ThemePicker,
    Edit,
    Insert,
    VisualChar,
    VisualLine,
    CommandInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOp {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone)]
struct Register {
    text: String,
    linewise: bool,
}

#[derive(Debug, Clone)]
enum LastChange {
    Insert(String),
    DeleteChars(usize),
    DeleteLines(usize),
    Paste { text: String, linewise: bool },
    ReplaceChar(char),
    ChangeLines { insert: String, count: usize },
}

struct LayoutInfo {
    main: Rect,
    status: Rect,
    outline: Option<Rect>,
    editor: Rect,
    preview: Option<Rect>,
    editor_width: u16,
    editor_height: u16,
    preview_width: Option<u16>,
    preview_height: Option<u16>,
}

struct App {
    file_path: PathBuf,
    config: Config,
    theme_manager: ThemeManager,
    syntax_set: SyntaxSet,
    parsed: ParsedDocument,
    rendered: RenderedDocument,
    ui: UiPalette,
    markdown_styles: MarkdownStyles,
    base_style: Style,
    source: String,
    scroll: usize,
    edit_scroll: usize,
    cursor_char: usize,
    preferred_col: Option<usize>,
    dirty: bool,
    render_dirty: bool,
    pending_op: Option<PendingOp>,
    pending_register: Option<char>,
    register_waiting: bool,
    registers: HashMap<char, Register>,
    count: Option<usize>,
    last_change: Option<LastChange>,
    undo_stack: Vec<Rope>,
    redo_stack: Vec<Rope>,
    insert_record: Option<String>,
    visual_anchor: Option<usize>,
    replace_pending: bool,
    pending_change_lines: Option<usize>,
    show_outline: bool,
    show_preview: bool,
    mode: Mode,
    search_query: String,
    search_input: String,
    command_input: String,
    current_match: usize,
    last_reload: SystemTime,
    last_width: u16,
    status: Option<String>,
    reload: FsReload,
    theme_selected: usize,
    theme_before_picker: Option<String>,
    suppress_reload_until: Option<Instant>,
    render_cursor_line: Option<usize>,
    editor_lines: Vec<Line<'static>>,
    editor_cache_dirty: bool,
    rope: Rope,
}

impl App {
    fn new(path: PathBuf, config: Config, theme_manager: ThemeManager) -> Result<Self> {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let markdown = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let theme = theme_manager.get(&config.theme);
        let ui = theme_manager.ui_palette(&config.theme);
        let (base_style, markdown_styles) = styles_from_palette(ui);
        let parsed = parse_markdown(
            &markdown,
            &syntax_set,
            theme,
            &markdown_styles,
            config.tab_width,
        )?;
        let rendered = wrap_document(&parsed, 80, None, config.search_case_sensitive);
        let rope = Rope::from_str(&markdown);
        let show_outline = config.show_outline;

        let theme_selected = theme_manager
            .theme_names()
            .iter()
            .position(|name| name == &config.theme)
            .unwrap_or(0);
        let mut registers = HashMap::new();
        registers.insert(
            '"',
            Register {
                text: String::new(),
                linewise: false,
            },
        );

        Ok(Self {
            file_path: path,
            config,
            theme_manager,
            syntax_set,
            parsed,
            rendered,
            ui,
            markdown_styles,
            base_style,
            source: markdown,
            scroll: 0,
            edit_scroll: 0,
            cursor_char: 0,
            preferred_col: None,
            dirty: false,
            render_dirty: false,
            pending_op: None,
            pending_register: None,
            register_waiting: false,
            registers,
            count: None,
            last_change: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            insert_record: None,
            visual_anchor: None,
            replace_pending: false,
            pending_change_lines: None,
            show_outline,
            show_preview: false,
            mode: Mode::Edit,
            search_query: String::new(),
            search_input: String::new(),
            command_input: String::new(),
            current_match: 0,
            last_reload: SystemTime::now(),
            last_width: 0,
            status: Some("NORMAL".to_string()),
            reload: FsReload::default(),
            theme_selected,
            theme_before_picker: None,
            suppress_reload_until: None,
            render_cursor_line: None,
            editor_lines: Vec::new(),
            editor_cache_dirty: true,
            rope,
        })
    }

    fn layout(&self, size: Rect) -> LayoutInfo {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(size);
        let main = vertical[0];
        let status = vertical[1];

        let (outline, body) = if self.show_outline {
            let outline_width = self
                .config
                .outline_width
                .min(main.width.saturating_sub(20));
            let horiz = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(outline_width), Constraint::Min(20)])
                .split(main);
            (Some(horiz[0]), horiz[1])
        } else {
            (None, main)
        };

        let (editor, preview) = if self.show_preview {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(body);
            (split[0], Some(split[1]))
        } else {
            (body, None)
        };

        let editor_width = editor.width.saturating_sub(2).max(1);
        let editor_height = editor.height.saturating_sub(2).max(1);
        let preview_width = preview.map(|p| p.width.saturating_sub(2).max(1));
        let preview_height = preview.map(|p| p.height.saturating_sub(2).max(1));

        LayoutInfo {
            main,
            status,
            outline,
            editor,
            preview,
            editor_width,
            editor_height,
            preview_width,
            preview_height,
        }
    }

    fn ensure_rendered(&mut self, width: u16) {
        if width == 0 {
            return;
        }
        if self.last_width != width {
            self.last_width = width;
            self.refresh_render(width);
        }
    }

    fn refresh_render(&mut self, width: u16) {
        let query = if self.search_query.is_empty() {
            None
        } else {
            Some(self.search_query.as_str())
        };
        let width = if self.config.wrap { width } else { u16::MAX };
        self.rendered = wrap_document(
            &self.parsed,
            width,
            query,
            self.config.search_case_sensitive,
        );
        self.render_cursor_line = None;
        if !self.rendered.matches.is_empty() && self.current_match >= self.rendered.matches.len() {
            self.current_match = 0;
        }
    }

    fn clamp_scroll(&mut self, height: u16) {
        let max_scroll = self
            .rendered
            .lines
            .len()
            .saturating_sub(height as usize);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    fn handle_key(&mut self, key: KeyEvent, content_height: u16) -> bool {
        match self.mode {
            Mode::SearchInput => return self.handle_search_input(key),
            Mode::ThemePicker => return self.handle_theme_picker(key),
            Mode::CommandInput => return self.handle_command_input(key),
            Mode::Normal | Mode::Edit | Mode::Insert | Mode::VisualChar | Mode::VisualLine => {
                return self.handle_editor_input(key, content_height)
            }
        }
    }

    fn handle_search_input(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Edit;
                self.search_input = String::new();
            }
            KeyCode::Enter => {
                self.mode = Mode::Edit;
                self.search_query = self.search_input.trim().to_string();
                self.search_input.clear();
                self.refresh_render(self.last_width.max(1));
                if self.rendered.matches.is_empty() && !self.search_query.is_empty() {
                    self.status = Some("No matches".to_string());
                } else if !self.rendered.matches.is_empty() {
                    self.current_match = 0;
                    self.scroll_to_match();
                }
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.push(c);
            }
            _ => {}
        }
        false
    }

    fn handle_theme_picker(&mut self, key: KeyEvent) -> bool {
        let total = self.theme_manager.theme_names().len();
        if total == 0 {
            self.mode = Mode::Edit;
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Edit;
                if let Some(original) = self.theme_before_picker.take() {
                    if self.config.theme != original {
                        self.config.theme = original;
                        self.apply_theme_styles();
                        self.reparse_with_theme(false);
                    }
                }
            }
            KeyCode::Up => {
                if self.theme_selected > 0 {
                    self.theme_selected -= 1;
                    self.preview_theme_selection();
                }
            }
            KeyCode::Down => {
                if self.theme_selected + 1 < total {
                    self.theme_selected += 1;
                    self.preview_theme_selection();
                }
            }
            KeyCode::PageUp => {
                self.theme_selected = self.theme_selected.saturating_sub(10);
                self.preview_theme_selection();
            }
            KeyCode::PageDown => {
                self.theme_selected = (self.theme_selected + 10).min(total - 1);
                self.preview_theme_selection();
            }
            KeyCode::Enter => {
                if let Some(theme) = self.theme_manager.theme_names().get(self.theme_selected) {
                    self.config.theme = theme.clone();
                    let _ = config::write_config(&self.config);
                    self.apply_theme_styles();
                    self.reparse_with_theme(true);
                }
                self.mode = Mode::Edit;
                self.theme_before_picker = None;
            }
            _ => {}
        }
        false
    }

    fn jump_heading(&mut self, delta: isize) {
        if self.rendered.headings.is_empty() {
            return;
        }
        let anchor = self
            .render_cursor_line
            .or_else(|| self.compute_rendered_cursor_line_col(self.scroll).map(|(line, _)| line))
            .unwrap_or(self.scroll);
        let current = current_heading_index(anchor, &self.rendered.headings);
        let next = match delta.cmp(&0) {
            Ordering::Less => current.saturating_sub(1),
            Ordering::Greater => (current + 1).min(self.rendered.headings.len() - 1),
            Ordering::Equal => current,
        };
        if let Some(h) = self.rendered.headings.get(next) {
            self.set_rendered_cursor_line(h.line);
        }
    }

    fn jump_match(&mut self, delta: isize) {
        if self.rendered.matches.is_empty() {
            return;
        }
        let len = self.rendered.matches.len();
        let idx = self.current_match as isize + delta;
        let next = if idx < 0 {
            len - 1
        } else {
            (idx as usize) % len
        };
        self.current_match = next;
        self.scroll_to_match();
    }

    fn scroll_to_match(&mut self) {
        if let Some(m) = self.rendered.matches.get(self.current_match) {
            self.set_rendered_cursor_line(m.line);
        }
    }

    fn request_reload(&mut self) {
        self.reload.pending = true;
        self.reload.deadline = Some(Instant::now() + Duration::from_millis(150));
    }

    fn on_fs_event(&mut self, _event: notify::Event) {
        if self.dirty
            || matches!(
                self.mode,
                Mode::Insert | Mode::VisualChar | Mode::VisualLine | Mode::CommandInput
            )
        {
            self.status = Some("External change ignored (editing)".to_string());
            return;
        }
        if let Some(until) = self.suppress_reload_until {
            if Instant::now() < until {
                return;
            }
            self.suppress_reload_until = None;
        }
        self.request_reload();
    }

    fn handle_pending_reload(&mut self) {
        if !self.reload.pending {
            return;
        }
        if let Some(deadline) = self.reload.deadline {
            if Instant::now() < deadline {
                return;
            }
        }
        self.reload.pending = false;
        self.reload.deadline = None;
        self.reload_file();
    }

    fn reload_file(&mut self) {
        let anchor = self
            .rendered
            .plain_lines
            .get(self.scroll)
            .cloned()
            .unwrap_or_default();
        let markdown = match fs::read_to_string(&self.file_path) {
            Ok(text) => text,
            Err(err) => {
                self.status = Some(format!("Failed to reload: {err}"));
                return;
            }
        };
        let theme = self.theme_manager.get(&self.config.theme);
        match parse_markdown(
            &markdown,
            &self.syntax_set,
            theme,
            &self.markdown_styles,
            self.config.tab_width,
        ) {
            Ok(parsed) => {
                self.source = markdown;
                self.rope = Rope::from_str(&self.source);
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.editor_cache_dirty = true;
                self.parsed = parsed;
                self.refresh_render(self.last_width.max(1));
                self.last_reload = SystemTime::now();
                self.render_dirty = false;
                self.render_cursor_line = None;
                self.status = Some("Reloaded".to_string());
                if let Some(idx) = find_anchor(&anchor, &self.rendered.plain_lines, self.scroll) {
                    self.scroll = idx;
                }
            }
            Err(err) => self.status = Some(format!("Parse error: {err}")),
        }
    }

    fn reparse_with_theme(&mut self, announce: bool) {
        let source = self.source.clone();
        self.reparse_with_text(&source, announce);
    }

    fn reparse_with_text(&mut self, text: &str, announce: bool) {
        let theme = self.theme_manager.get(&self.config.theme);
        match parse_markdown(
            text,
            &self.syntax_set,
            theme,
            &self.markdown_styles,
            self.config.tab_width,
        ) {
            Ok(parsed) => {
                self.parsed = parsed;
                self.refresh_render(self.last_width.max(1));
                self.render_cursor_line = None;
                if announce {
                    self.status = Some(format!("Theme: {}", self.config.theme));
                }
            }
            Err(err) => self.status = Some(format!("Parse error: {err}")),
        }
    }

    fn apply_theme_styles(&mut self) {
        self.ui = self.theme_manager.ui_palette(&self.config.theme);
        let (base_style, markdown_styles) = styles_from_palette(self.ui);
        self.base_style = base_style;
        self.markdown_styles = markdown_styles;
        self.editor_cache_dirty = true;
    }

    fn mark_render_dirty(&mut self) {
        self.render_dirty = true;
        self.editor_cache_dirty = true;
    }

    fn sync_render_from_rope(&mut self) {
        if !self.render_dirty {
            return;
        }
        let text = self.rope.to_string();
        self.reparse_with_text(&text, false);
        let max_scroll = self.rendered.lines.len().saturating_sub(1);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        self.render_cursor_line = None;
        self.render_dirty = false;
    }

    fn preview_theme_selection(&mut self) {
        if let Some(theme) = self.theme_manager.theme_names().get(self.theme_selected) {
            if self.config.theme != *theme {
                self.config.theme = theme.clone();
                self.apply_theme_styles();
                self.reparse_with_theme(false);
            }
        }
    }

    fn handle_editor_input(&mut self, key: KeyEvent, content_height: u16) -> bool {
        match self.mode {
            Mode::Normal | Mode::Edit => self.handle_normal_mode(key, content_height),
            Mode::Insert => self.handle_insert_mode(key, content_height),
            Mode::VisualChar | Mode::VisualLine => self.handle_visual_mode(key, content_height),
            _ => false,
        }
    }

    fn handle_normal_mode(&mut self, key: KeyEvent, content_height: u16) -> bool {
        if self.replace_pending {
            self.replace_pending = false;
            if let KeyCode::Char(c) = key.code {
                self.replace_char(c);
            }
            return false;
        }
        if self.consume_register_wait(key) {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    let half = (content_height / 2).max(1) as isize;
                    let count = self.take_count() as isize;
                    self.move_cursor_page(half.saturating_mul(count.max(1)));
                    return false;
                }
                KeyCode::Char('u') => {
                    let half = (content_height / 2).max(1) as isize;
                    let count = self.take_count() as isize;
                    self.move_cursor_page(-half.saturating_mul(count.max(1)));
                    return false;
                }
                _ => {}
            }
        }

        if let KeyCode::Char(c) = key.code {
            if let Some(digit) = c.to_digit(10) {
                if digit == 0 && self.count.is_none() {
                    self.move_cursor_line_start();
                    self.ensure_cursor_visible(content_height);
                    return false;
                }
                self.push_count(digit as usize);
                return false;
            }
        }

        if let Some(op) = self.pending_op {
            self.pending_op = None;
            if matches!(
                (op, key.code),
                (PendingOp::Delete, KeyCode::Char('d'))
                    | (PendingOp::Change, KeyCode::Char('c'))
                    | (PendingOp::Yank, KeyCode::Char('y'))
            ) {
                let count = self.take_count();
                match op {
                    PendingOp::Delete => self.delete_lines(count),
                    PendingOp::Change => {
                        self.change_lines(count);
                    }
                    PendingOp::Yank => self.yank_lines(count),
                }
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                self.clear_pending();
            }
            KeyCode::Char('q') => {
                if self.dirty {
                    self.status =
                        Some("No write since last change (use :q! to discard)".to_string());
                } else {
                    return true;
                }
            }
            KeyCode::Char('"') => {
                self.register_waiting = true;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let count = self.take_count();
                for _ in 0..count {
                    self.move_cursor_left();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.take_count();
                for _ in 0..count {
                    self.move_cursor_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let count = self.take_count();
                for _ in 0..count {
                    self.move_cursor_up();
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let count = self.take_count();
                for _ in 0..count {
                    self.move_cursor_right();
                }
            }
            KeyCode::PageUp => self.move_cursor_page(-(content_height as isize)),
            KeyCode::PageDown => self.move_cursor_page(content_height as isize),
            KeyCode::Char('i') => self.enter_insert_mode(),
            KeyCode::Char('a') => {
                if !self.is_at_line_end() {
                    self.move_cursor_right();
                }
                self.enter_insert_mode();
            }
            KeyCode::Char('I') => {
                self.move_cursor_first_non_ws();
                self.enter_insert_mode();
            }
            KeyCode::Char('A') => {
                self.move_cursor_line_end();
                self.enter_insert_mode();
            }
            KeyCode::Char('o') => {
                self.open_line_below();
                self.enter_insert_mode();
            }
            KeyCode::Char('O') => {
                self.open_line_above();
                self.enter_insert_mode();
            }
            KeyCode::Char('v') => self.enter_visual_char(),
            KeyCode::Char('V') => self.enter_visual_line(),
            KeyCode::Char('x') => {
                let count = self.take_count();
                self.delete_chars(count);
            }
            KeyCode::Char('d') => {
                self.pending_op = Some(PendingOp::Delete);
            }
            KeyCode::Char('c') => {
                self.pending_op = Some(PendingOp::Change);
            }
            KeyCode::Char('y') => {
                self.pending_op = Some(PendingOp::Yank);
            }
            KeyCode::Char('p') => {
                let count = self.take_count();
                self.paste_after(count);
            }
            KeyCode::Char('P') => {
                let count = self.take_count();
                self.paste_before(count);
            }
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('.') => self.repeat_last_change(),
            KeyCode::Char('r') => {
                self.replace_pending = true;
            }
            KeyCode::Char('R') => {
                self.request_reload();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::CommandInput;
                self.command_input.clear();
                return false;
            }
            KeyCode::Char('B') => {
                self.show_preview = !self.show_preview;
            }
            KeyCode::Char('H') => {
                self.show_outline = !self.show_outline;
            }
            KeyCode::Char('[') => self.jump_heading(-1),
            KeyCode::Char(']') => self.jump_heading(1),
            KeyCode::Char('g') => {
                self.move_cursor_file_start();
            }
            KeyCode::Char('G') => {
                self.move_cursor_file_end();
            }
            KeyCode::Char('/') => {
                self.search_input = self.search_query.clone();
                self.mode = Mode::SearchInput;
            }
            KeyCode::Char('n') => self.jump_match(1),
            KeyCode::Char('N') => self.jump_match(-1),
            KeyCode::Char('t') => {
                self.mode = Mode::ThemePicker;
                self.theme_before_picker = Some(self.config.theme.clone());
                self.theme_selected = self
                    .theme_manager
                    .theme_names()
                    .iter()
                    .position(|name| name == &self.config.theme)
                    .unwrap_or(0);
            }
            _ => {}
        }

        if self.pending_op.is_none() && !self.register_waiting {
            self.count = None;
        }
        self.ensure_cursor_visible(content_height);
        if self.show_preview {
            self.update_render_cursor_line();
            self.ensure_rendered_cursor_visible(content_height);
        }
        false
    }

    fn handle_insert_mode(&mut self, key: KeyEvent, content_height: u16) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_buffer();
                    return false;
                }
                KeyCode::Char('r') => {
                    self.redo();
                    return false;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.exit_insert_mode();
            }
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Up => self.move_cursor_up(),
            KeyCode::Down => self.move_cursor_down(),
            KeyCode::PageUp => self.move_cursor_page(-(content_height as isize)),
            KeyCode::PageDown => self.move_cursor_page(content_height as isize),
            KeyCode::Home => self.move_cursor_line_start(),
            KeyCode::End => self.move_cursor_line_end(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Enter => self.insert_char('\n'),
            KeyCode::Tab => {
                let spaces = " ".repeat(self.config.tab_width.max(1));
                self.insert_str(&spaces);
            }
            KeyCode::Char(c) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.insert_char(c);
                }
            }
            _ => {}
        }

        self.ensure_cursor_visible(content_height);
        if self.show_preview {
            self.sync_render_from_rope();
            self.update_render_cursor_line();
            self.ensure_rendered_cursor_visible(content_height);
        }
        false
    }

    fn handle_visual_mode(&mut self, key: KeyEvent, content_height: u16) -> bool {
        if self.consume_register_wait(key) {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.exit_visual_mode();
            }
            KeyCode::Char('h') | KeyCode::Left => self.move_cursor_left(),
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor_up(),
            KeyCode::Char('l') | KeyCode::Right => self.move_cursor_right(),
            KeyCode::Char('0') => self.move_cursor_line_start(),
            KeyCode::Char('$') => self.move_cursor_line_end(),
            KeyCode::Char('d') => {
                self.delete_selection();
                self.exit_visual_mode();
            }
            KeyCode::Char('y') => {
                self.yank_selection();
                self.exit_visual_mode();
            }
            KeyCode::Char('c') => {
                self.delete_selection();
                self.exit_visual_mode();
                self.enter_insert_mode();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::CommandInput;
                self.command_input.clear();
            }
            _ => {}
        }
        self.ensure_cursor_visible(content_height);
        if self.show_preview {
            self.sync_render_from_rope();
            self.update_render_cursor_line();
            self.ensure_rendered_cursor_visible(content_height);
        }
        false
    }

    fn handle_command_input(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Edit;
                self.command_input.clear();
            }
            KeyCode::Enter => {
                let command = self.command_input.trim().to_string();
                self.command_input.clear();
                self.execute_command(&command);
            }
            KeyCode::Backspace => {
                self.command_input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command_input.push(c);
            }
            _ => {}
        }
        if matches!(self.mode, Mode::Edit | Mode::Normal) {
            self.sync_render_from_rope();
        }
        false
    }

    fn insert_char(&mut self, c: char) {
        self.rope.insert_char(self.cursor_char, c);
        self.cursor_char = self.cursor_char.saturating_add(1);
        self.preferred_col = None;
        if let Some(record) = self.insert_record.as_mut() {
            record.push(c);
        }
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn insert_str(&mut self, text: &str) {
        self.rope.insert(self.cursor_char, text);
        self.cursor_char = self.cursor_char.saturating_add(text.chars().count());
        self.preferred_col = None;
        if let Some(record) = self.insert_record.as_mut() {
            record.push_str(text);
        }
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_char == 0 {
            return;
        }
        let prev = self.cursor_char - 1;
        self.rope.remove(prev..self.cursor_char);
        self.cursor_char = prev;
        self.preferred_col = None;
        if let Some(record) = self.insert_record.as_mut() {
            if !record.is_empty() {
                record.pop();
            }
        }
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn delete(&mut self) {
        if self.cursor_char >= self.rope.len_chars() {
            return;
        }
        let next = self.cursor_char + 1;
        self.rope.remove(self.cursor_char..next);
        self.preferred_col = None;
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_char > 0 {
            self.cursor_char -= 1;
        }
        self.preferred_col = None;
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_char < self.rope.len_chars() {
            self.cursor_char += 1;
        }
        self.preferred_col = None;
    }

    fn move_cursor_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        let target_line = line - 1;
        let desired = self.preferred_col.unwrap_or(col);
        let target_col = desired.min(line_len_chars(&self.rope, target_line));
        self.cursor_char = self.rope.line_to_char(target_line) + target_col;
        self.preferred_col = Some(desired);
    }

    fn move_cursor_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        let max_line = self.rope.len_lines().saturating_sub(1);
        if line >= max_line {
            return;
        }
        let target_line = line + 1;
        let desired = self.preferred_col.unwrap_or(col);
        let target_col = desired.min(line_len_chars(&self.rope, target_line));
        self.cursor_char = self.rope.line_to_char(target_line) + target_col;
        self.preferred_col = Some(desired);
    }

    fn move_cursor_page(&mut self, delta: isize) {
        let (line, col) = self.cursor_line_col();
        let max_line = self.rope.len_lines().saturating_sub(1);
        let target_line = if delta.is_negative() {
            line.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (line + delta as usize).min(max_line)
        };
        let desired = self.preferred_col.unwrap_or(col);
        let target_col = desired.min(line_len_chars(&self.rope, target_line));
        self.cursor_char = self.rope.line_to_char(target_line) + target_col;
        self.preferred_col = Some(desired);
    }

    fn move_cursor_line_start(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor_char = self.rope.line_to_char(line);
        self.preferred_col = None;
    }

    fn move_cursor_line_end(&mut self) {
        let (line, _) = self.cursor_line_col();
        let len = line_len_chars(&self.rope, line);
        self.cursor_char = self.rope.line_to_char(line) + len;
        self.preferred_col = None;
    }

    fn move_cursor_file_start(&mut self) {
        self.cursor_char = 0;
        self.edit_scroll = 0;
        self.preferred_col = None;
    }

    fn move_cursor_file_end(&mut self) {
        if self.rope.len_chars() == 0 {
            self.cursor_char = 0;
            return;
        }
        self.cursor_char = self.rope.len_chars().saturating_sub(1);
        let line = self.rope.char_to_line(self.cursor_char);
        self.edit_scroll = line;
        self.preferred_col = None;
    }

    fn set_rendered_cursor_line(&mut self, line: usize) {
        if self.rendered.plain_lines.is_empty() {
            return;
        }
        let target = line.min(self.rendered.plain_lines.len().saturating_sub(1));
        self.render_cursor_line = Some(target);
        self.move_cursor_to_rendered_line(target);
    }

    fn ensure_cursor_visible(&mut self, height: u16) {
        let (line, _) = self.cursor_line_col();
        let height = height as usize;
        if line < self.edit_scroll {
            self.edit_scroll = line;
        } else if line >= self.edit_scroll + height {
            self.edit_scroll = line.saturating_sub(height.saturating_sub(1));
        }
    }

    fn ensure_rendered_cursor_visible(&mut self, height: u16) {
        let line = if let Some(line) = self.render_cursor_line {
            line
        } else if let Some((line, _)) = self.compute_rendered_cursor_line_col(self.scroll) {
            self.render_cursor_line = Some(line);
            line
        } else {
            return;
        };
        let height = height as usize;
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + height {
            self.scroll = line.saturating_sub(height.saturating_sub(1));
        }
    }

    fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.rope.char_to_line(self.cursor_char);
        let line_start = self.rope.line_to_char(line);
        let col = self.cursor_char.saturating_sub(line_start);
        (line, col)
    }

    fn update_render_cursor_line(&mut self) {
        if self.rendered.plain_lines.is_empty() {
            return;
        }
        let anchor = self.render_cursor_line.unwrap_or(self.scroll);
        if let Some((line, _)) = self.compute_rendered_cursor_line_col(anchor) {
            self.render_cursor_line = Some(line);
        }
    }

    fn compute_rendered_cursor_line_col(&self, anchor: usize) -> Option<(usize, usize)> {
        if self.rendered.plain_lines.is_empty() {
            return None;
        }
        let (src_line, src_col) = self.cursor_line_col();
        let mut line_str = self.rope.line(src_line).to_string();
        if line_str.ends_with('\n') {
            line_str.pop();
            if line_str.ends_with('\r') {
                line_str.pop();
            }
        }

        let normalized = normalize_markdown_line(&line_str);
        let visible_col = normalize_prefix_for_col(&line_str, src_col);
        let anchor = anchor.min(self.rendered.plain_lines.len().saturating_sub(1));
        if normalized.trim().is_empty() {
            return Some((anchor, 0));
        }

        let candidates = build_match_candidates(&normalized, visible_col);
        if let Some((line, match_col)) = self.find_best_rendered_match(&candidates, anchor) {
            let line_len = self
                .rendered
                .plain_lines
                .get(line)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            let col = match_col.unwrap_or(visible_col).min(line_len);
            return Some((line, col));
        }

        let fallback = src_line.min(self.rendered.plain_lines.len().saturating_sub(1));
        let line_len = self
            .rendered
            .plain_lines
            .get(fallback)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        Some((fallback, visible_col.min(line_len)))
    }

    fn find_best_rendered_match(
        &self,
        candidates: &[String],
        anchor: usize,
    ) -> Option<(usize, Option<usize>)> {
        if candidates.is_empty() {
            return None;
        }
        let total = self.rendered.plain_lines.len();
        if total == 0 {
            return None;
        }
        let anchor = anchor.min(total - 1);
        let window = 200usize;
        let ranges = [
            (
                anchor.saturating_sub(window),
                (anchor + window + 1).min(total),
            ),
            (0, total),
        ];

        let mut best: Option<(usize, i64, Option<usize>)> = None;
        for (start, end) in ranges {
            for idx in start..end {
                let line = &self.rendered.plain_lines[idx];
                if let Some((len, col)) = match_line(line, candidates) {
                    let dist = if idx > anchor {
                        idx - anchor
                    } else {
                        anchor - idx
                    };
                    let score = dist as i64 * 1000 - len as i64;
                    let replace = match best {
                        Some((_, best_score, _)) => score < best_score,
                        None => true,
                    };
                    if replace {
                        best = Some((idx, score, col));
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }
        best.map(|(idx, _score, col)| (idx, col))
    }

    fn move_cursor_to_rendered_line(&mut self, rendered_line: usize) {
        let Some(text) = self.rendered.plain_lines.get(rendered_line) else {
            return;
        };
        let anchor = self.rope.char_to_line(self.cursor_char);
        if let Some(src_line) = self.find_source_line_for_text(text, anchor) {
            self.cursor_char = self.rope.line_to_char(src_line);
            self.preferred_col = None;
            self.render_cursor_line = Some(rendered_line);
        }
    }

    fn find_source_line_for_text(&self, text: &str, anchor: usize) -> Option<usize> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let candidates = build_match_candidates(trimmed, 0);
        if candidates.is_empty() {
            return None;
        }

        let total = self.rope.len_lines();
        if total == 0 {
            return None;
        }
        let anchor = anchor.min(total.saturating_sub(1));
        let mut best: Option<(usize, i64)> = None;

        for line_idx in 0..total {
            let mut line_str = self.rope.line(line_idx).to_string();
            if line_str.ends_with('\n') {
                line_str.pop();
                if line_str.ends_with('\r') {
                    line_str.pop();
                }
            }
            let normalized = normalize_markdown_line(&line_str);
            if let Some((len, _)) = match_line(&normalized, &candidates) {
                let dist = if line_idx > anchor {
                    line_idx - anchor
                } else {
                    anchor - line_idx
                };
                let score = dist as i64 * 1000 - len as i64;
                let replace = match best {
                    Some((_, best_score)) => score < best_score,
                    None => true,
                };
                if replace {
                    best = Some((line_idx, score));
                }
            }
        }

        best.map(|(idx, _)| idx)
    }

    fn save_buffer(&mut self) {
        let text = self.rope.to_string();
        if let Err(err) = fs::write(&self.file_path, &text) {
            self.status = Some(format!("Save failed: {err}"));
            return;
        }
        self.source = text;
        self.render_cursor_line = None;
        self.dirty = false;
        self.suppress_reload_until = Some(Instant::now() + Duration::from_millis(300));
        self.status = Some("Saved".to_string());
    }

    fn execute_command(&mut self, command: &str) {
        let cmd = command.trim();
        if cmd.is_empty() {
            self.mode = Mode::Edit;
            return;
        }

        match cmd {
            "w" | "write" | "w!" => {
                self.save_buffer();
                self.mode = Mode::Edit;
            }
            "wq" | "x" => {
                self.save_buffer();
                if !self.dirty {
                    self.exit_edit_mode();
                } else {
                    self.mode = Mode::Edit;
                }
            }
            "q" | "quit" => {
                if self.dirty {
                    self.status = Some("No write since last change (add ! to override)".to_string());
                    self.mode = Mode::Edit;
                } else {
                    self.exit_edit_mode();
                }
            }
            "q!" | "quit!" => {
                self.discard_changes();
                self.exit_edit_mode();
            }
            _ => {
                self.status = Some(format!("Not an editor command: {cmd}"));
                self.mode = Mode::Edit;
            }
        }
    }

    fn clear_pending(&mut self) {
        self.pending_op = None;
        self.count = None;
        self.pending_register = None;
        self.register_waiting = false;
        self.replace_pending = false;
    }

    fn push_count(&mut self, digit: usize) {
        let next = self.count.unwrap_or(0) * 10 + digit;
        self.count = Some(next);
    }

    fn take_count(&mut self) -> usize {
        let count = self.count.unwrap_or(1);
        self.count = None;
        count
    }

    fn consume_register_wait(&mut self, key: KeyEvent) -> bool {
        if !self.register_waiting {
            return false;
        }
        self.register_waiting = false;
        if let KeyCode::Char(c) = key.code {
            self.pending_register = Some(c);
        }
        true
    }

    fn consume_active_register(&mut self) -> char {
        self.pending_register.take().unwrap_or('"')
    }

    fn set_register(&mut self, text: String, linewise: bool, is_yank: bool) {
        let reg = Register { text: text.clone(), linewise };
        let target = self.consume_active_register();
        self.registers.insert(target, reg.clone());
        self.registers.insert('"', reg.clone());
        if is_yank {
            self.registers.insert('0', reg);
        }
    }

    fn enter_insert_mode(&mut self) {
        if self.mode != Mode::Insert {
            self.push_undo();
            self.insert_record = Some(String::new());
        }
        self.mode = Mode::Insert;
        self.status = Some("INSERT".to_string());
        self.clear_pending();
    }

    fn exit_insert_mode(&mut self) {
        if self.cursor_char > 0 {
            self.cursor_char = self.cursor_char.saturating_sub(1);
        }
        let pending_change = self.pending_change_lines.take();
        if let Some(record) = self.insert_record.take() {
            if !record.is_empty() {
                if let Some(count) = pending_change {
                    self.last_change = Some(LastChange::ChangeLines {
                        insert: record,
                        count,
                    });
                } else {
                    self.last_change = Some(LastChange::Insert(record));
                }
            }
        }
        self.mode = Mode::Edit;
        self.status = Some("NORMAL".to_string());
        self.clear_pending();
    }

    fn enter_visual_char(&mut self) {
        self.mode = Mode::VisualChar;
        self.visual_anchor = Some(self.cursor_char);
        self.status = Some("VISUAL".to_string());
        self.clear_pending();
    }

    fn enter_visual_line(&mut self) {
        self.mode = Mode::VisualLine;
        self.visual_anchor = Some(self.cursor_char);
        self.status = Some("VISUAL LINE".to_string());
        self.clear_pending();
    }

    fn exit_visual_mode(&mut self) {
        self.mode = Mode::Edit;
        self.visual_anchor = None;
        self.status = Some("NORMAL".to_string());
        self.clear_pending();
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(self.rope.clone());
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.rope.clone());
            self.rope = prev;
            self.cursor_char = self.cursor_char.min(self.rope.len_chars());
            self.mark_render_dirty();
            self.update_dirty();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.rope.clone());
            self.rope = next;
            self.cursor_char = self.cursor_char.min(self.rope.len_chars());
            self.mark_render_dirty();
            self.update_dirty();
        }
    }

    fn update_dirty(&mut self) {
        self.dirty = self.rope.to_string() != self.source;
    }

    fn is_at_line_end(&self) -> bool {
        let (line, col) = self.cursor_line_col();
        col >= line_len_chars(&self.rope, line)
    }

    fn move_cursor_first_non_ws(&mut self) {
        let (line, _) = self.cursor_line_col();
        let line_str = self.rope.line(line).to_string();
        let mut idx = 0usize;
        for ch in line_str.chars() {
            if ch == '\n' || ch == '\r' {
                break;
            }
            if !ch.is_whitespace() {
                break;
            }
            idx += 1;
        }
        self.cursor_char = self.rope.line_to_char(line) + idx;
        self.preferred_col = None;
    }

    fn open_line_below(&mut self) {
        let line = self.rope.char_to_line(self.cursor_char);
        let insert_at = if line + 1 >= self.rope.len_lines() {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(line + 1)
        };
        self.push_undo();
        self.rope.insert(insert_at, "\n");
        self.cursor_char = insert_at + 1;
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn open_line_above(&mut self) {
        let line = self.rope.char_to_line(self.cursor_char);
        let insert_at = self.rope.line_to_char(line);
        self.push_undo();
        self.rope.insert(insert_at, "\n");
        self.cursor_char = insert_at;
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn delete_chars(&mut self, count: usize) {
        if count == 0 || self.cursor_char >= self.rope.len_chars() {
            return;
        }
        let end = (self.cursor_char + count).min(self.rope.len_chars());
        self.push_undo();
        let text = self.rope.slice(self.cursor_char..end).to_string();
        self.set_register(text.clone(), false, false);
        self.rope.remove(self.cursor_char..end);
        self.last_change = Some(LastChange::DeleteChars(count));
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn line_range(&self, start_line: usize, count: usize) -> (usize, usize) {
        let start = self.rope.line_to_char(start_line);
        let end_line = (start_line + count).min(self.rope.len_lines());
        let end = if end_line >= self.rope.len_lines() {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(end_line)
        };
        (start, end)
    }

    fn delete_lines(&mut self, count: usize) {
        let line = self.rope.char_to_line(self.cursor_char);
        let count = count.max(1);
        let (start, end) = self.line_range(line, count);
        if start == end {
            return;
        }
        self.push_undo();
        let text = self.rope.slice(start..end).to_string();
        self.set_register(text, true, false);
        self.rope.remove(start..end);
        self.cursor_char = self.rope.line_to_char(line.min(self.rope.len_lines().saturating_sub(1)));
        self.last_change = Some(LastChange::DeleteLines(count));
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn yank_lines(&mut self, count: usize) {
        let line = self.rope.char_to_line(self.cursor_char);
        let count = count.max(1);
        let (start, end) = self.line_range(line, count);
        let text = self.rope.slice(start..end).to_string();
        self.set_register(text, true, true);
        self.status = Some(format!("Yanked {count} line(s)"));
    }

    fn change_lines(&mut self, count: usize) {
        self.delete_lines(count);
        self.pending_change_lines = Some(count);
        self.enter_insert_mode();
    }

    fn paste_after(&mut self, count: usize) {
        let reg_char = self.consume_active_register();
        let reg = match self
            .registers
            .get(&reg_char)
            .cloned()
            .or_else(|| self.registers.get(&'"').cloned())
        {
            Some(r) => r,
            None => return,
        };
        if reg.text.is_empty() {
            return;
        }
        self.push_undo();
        if reg.linewise {
            let line = self.rope.char_to_line(self.cursor_char);
            let insert_at = if line + 1 >= self.rope.len_lines() {
                self.rope.len_chars()
            } else {
                self.rope.line_to_char(line + 1)
            };
            for _ in 0..count {
                self.rope.insert(insert_at, &reg.text);
            }
            self.cursor_char = insert_at;
        } else {
            let mut insert_at = (self.cursor_char + 1).min(self.rope.len_chars());
            for _ in 0..count {
                self.rope.insert(insert_at, &reg.text);
                insert_at += reg.text.chars().count();
            }
            self.cursor_char = insert_at.saturating_sub(1);
        }
        self.last_change = Some(LastChange::Paste {
            text: reg.text,
            linewise: reg.linewise,
        });
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn paste_before(&mut self, count: usize) {
        let reg_char = self.consume_active_register();
        let reg = match self
            .registers
            .get(&reg_char)
            .cloned()
            .or_else(|| self.registers.get(&'"').cloned())
        {
            Some(r) => r,
            None => return,
        };
        if reg.text.is_empty() {
            return;
        }
        self.push_undo();
        if reg.linewise {
            let line = self.rope.char_to_line(self.cursor_char);
            let insert_at = self.rope.line_to_char(line);
            for _ in 0..count {
                self.rope.insert(insert_at, &reg.text);
            }
            self.cursor_char = insert_at;
        } else {
            let mut insert_at = self.cursor_char;
            for _ in 0..count {
                self.rope.insert(insert_at, &reg.text);
                insert_at += reg.text.chars().count();
            }
            self.cursor_char = insert_at.saturating_sub(1);
        }
        self.last_change = Some(LastChange::Paste {
            text: reg.text,
            linewise: reg.linewise,
        });
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn selection_range(&self) -> Option<(usize, usize, bool)> {
        let anchor = self.visual_anchor?;
        let cursor = self.cursor_char;
        if matches!(self.mode, Mode::VisualLine) {
            let start_line = self.rope.char_to_line(anchor.min(cursor));
            let end_line = self.rope.char_to_line(anchor.max(cursor));
            let (start, end) = self.line_range(start_line, end_line - start_line + 1);
            return Some((start, end, true));
        }
        let start = anchor.min(cursor);
        let mut end = anchor.max(cursor);
        end = end.saturating_add(1);
        end = end.min(self.rope.len_chars());
        Some((start, end, false))
    }

    fn yank_selection(&mut self) {
        if let Some((start, end, linewise)) = self.selection_range() {
            let text = self.rope.slice(start..end).to_string();
            self.set_register(text, linewise, true);
        }
    }

    fn delete_selection(&mut self) {
        if let Some((start, end, linewise)) = self.selection_range() {
            self.push_undo();
            let text = self.rope.slice(start..end).to_string();
            let line_count = if linewise {
                let start_line = self.rope.char_to_line(start);
                let end_line = self.rope.char_to_line(end.saturating_sub(1));
                end_line.saturating_sub(start_line).saturating_add(1)
            } else {
                0
            };
            let char_count = if linewise { 0 } else { end.saturating_sub(start).max(1) };
            self.set_register(text, linewise, false);
            self.rope.remove(start..end);
            self.cursor_char = start.min(self.rope.len_chars());
            if linewise {
                self.last_change = Some(LastChange::DeleteLines(line_count.max(1)));
            } else {
                self.last_change = Some(LastChange::DeleteChars(char_count.max(1)));
            }
            self.mark_render_dirty();
            self.dirty = true;
        }
    }

    fn replace_char(&mut self, c: char) {
        if self.cursor_char >= self.rope.len_chars() {
            return;
        }
        self.push_undo();
        self.rope.remove(self.cursor_char..self.cursor_char + 1);
        self.rope.insert_char(self.cursor_char, c);
        self.last_change = Some(LastChange::ReplaceChar(c));
        self.mark_render_dirty();
        self.dirty = true;
    }

    fn repeat_last_change(&mut self) {
        let change = match self.last_change.clone() {
            Some(c) => c,
            None => return,
        };
        match change {
            LastChange::Insert(text) => {
                if text.is_empty() {
                    return;
                }
                self.push_undo();
                let insert_at = (self.cursor_char + 1).min(self.rope.len_chars());
                self.rope.insert(insert_at, &text);
                self.cursor_char = insert_at + text.chars().count().saturating_sub(1);
                self.mark_render_dirty();
                self.dirty = true;
            }
            LastChange::DeleteChars(count) => {
                self.delete_chars(count);
            }
            LastChange::DeleteLines(count) => {
                self.delete_lines(count);
            }
            LastChange::Paste { text, linewise } => {
                self.pending_register = Some('"');
                self.registers.insert(
                    '"',
                    Register {
                        text: text.clone(),
                        linewise,
                    },
                );
                self.paste_after(1);
            }
            LastChange::ReplaceChar(c) => {
                self.replace_char(c);
            }
            LastChange::ChangeLines { insert, count } => {
                self.delete_lines(count);
                if insert.is_empty() {
                    return;
                }
                self.rope.insert(self.cursor_char, &insert);
                self.cursor_char = self.cursor_char + insert.chars().count().saturating_sub(1);
                self.last_change = Some(LastChange::ChangeLines { insert, count });
                self.mark_render_dirty();
                self.dirty = true;
            }
        }
    }

    fn discard_changes(&mut self) {
        self.rope = Rope::from_str(&self.source);
        self.cursor_char = self.cursor_char.min(self.rope.len_chars());
        self.mark_render_dirty();
        self.dirty = false;
    }

    fn exit_edit_mode(&mut self) {
        self.mode = Mode::Normal;
        self.visual_anchor = None;
        self.insert_record = None;
        self.pending_change_lines = None;
        self.clear_pending();
        self.source = self.rope.to_string();
        self.reparse_with_theme(false);
        self.render_dirty = false;
        self.scroll = self.edit_scroll;
        self.status = if self.dirty {
            Some("Modified".to_string())
        } else {
            Some("View".to_string())
        };
    }
}

fn current_heading_index(scroll: usize, headings: &[Heading]) -> usize {
    let mut idx = 0;
    for (i, h) in headings.iter().enumerate() {
        if h.line <= scroll {
            idx = i;
        } else {
            break;
        }
    }
    idx
}

fn find_anchor(anchor: &str, lines: &[String], prev_scroll: usize) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line == anchor {
            let dist = if idx > prev_scroll {
                idx - prev_scroll
            } else {
                prev_scroll - idx
            };
            match best {
                Some((_, best_dist)) if dist >= best_dist => {}
                _ => best = Some((idx, dist)),
            }
        }
    }
    best.map(|(idx, _)| idx)
}

fn ui(f: &mut ratatui::Frame, app: &mut App, layout: &LayoutInfo) {
    let highlight_fg = app.ui.base_bg.unwrap_or(app.ui.base_fg);
    let highlight_style = Style::default().bg(app.ui.accent).fg(highlight_fg);

    let status_line = app.status_line();
    f.render_widget(
        Paragraph::new(status_line)
            .style(app.base_style)
            .block(Block::default().style(app.base_style)),
        layout.status,
    );

    if let Some(outline_area) = layout.outline {
        let items: Vec<ListItem> = app
            .rendered
            .headings
            .iter()
            .map(|h| {
                let indent = "  ".repeat((h.level.saturating_sub(1)) as usize);
                ListItem::new(format!("{indent}{}", h.title))
            })
            .collect();
        let mut state = ListState::default();
        let selected = current_heading_index(app.scroll, &app.rendered.headings);
        state.select(Some(selected));
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title("Outline")
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(app.ui.border))
                    .style(app.base_style),
            )
            .style(app.base_style)
            .highlight_style(highlight_style);
        f.render_stateful_widget(list, outline_area, &mut state);
    }

    let file_name = app
        .file_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("mark");
    let title = if app.dirty {
        format!(" *{file_name} ")
    } else {
        format!(" {file_name} ")
    };

    let editor_text = if matches!(app.mode, Mode::VisualChar | Mode::VisualLine) {
        app.edit_text()
    } else {
        app.editor_text()
    };
    let editor_paragraph = Paragraph::new(editor_text)
        .block(
            Block::bordered()
                .title(title)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(app.ui.border))
                .style(app.base_style),
        )
        .style(app.base_style)
        .scroll((app.edit_scroll as u16, 0));
    f.render_widget(editor_paragraph, layout.editor);

    if let Some(preview_area) = layout.preview {
        let preview_paragraph = Paragraph::new(Text::from(app.rendered.lines.clone()))
            .block(
                Block::bordered()
                    .title(" Preview ")
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(app.ui.border))
                    .style(app.base_style),
            )
            .style(app.base_style)
            .scroll((app.scroll as u16, 0));
        f.render_widget(preview_paragraph, preview_area);
    }

    if matches!(app.mode, Mode::ThemePicker) {
        let popup = centered_rect(60, 70, layout.main);
        f.render_widget(Clear, popup);
        let items: Vec<ListItem> = app
            .theme_manager
            .theme_names()
            .iter()
            .map(|name| ListItem::new(name.clone()))
            .collect();
        let mut state = ListState::default();
        state.select(Some(app.theme_selected));
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title("Themes")
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(app.ui.border))
                    .style(app.base_style),
            )
            .style(app.base_style)
            .highlight_style(highlight_style);
        f.render_stateful_widget(list, popup, &mut state);
    }

    if let Some((x, y)) = app.cursor_screen_position(layout) {
        f.set_cursor(x, y);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

impl App {
    fn status_line(&self) -> Line<'static> {
        if matches!(self.mode, Mode::SearchInput) {
            return Line::from(vec![
                Span::styled("/", Style::default().fg(self.ui.accent)),
                Span::styled(self.search_input.clone(), self.base_style),
            ]);
        }

        let mut parts = Vec::new();
        parts.push(
            Span::styled("mark", Style::default().fg(self.ui.accent).add_modifier(Modifier::BOLD)),
        );
        if matches!(self.mode, Mode::CommandInput) {
            return Line::from(vec![
                Span::styled(":", Style::default().fg(self.ui.accent)),
                Span::styled(self.command_input.clone(), self.base_style),
            ]);
        }
        parts.push(Span::styled(" | ", Style::default().fg(self.ui.muted)));
        let mode_label = match self.mode {
            Mode::Normal => "normal",
            Mode::SearchInput => "search",
            Mode::ThemePicker => "themes",
            Mode::Edit => "normal",
            Mode::Insert => "insert",
            Mode::VisualChar => "visual",
            Mode::VisualLine => "visual-line",
            Mode::CommandInput => "cmd",
        };
        parts.push(Span::styled(mode_label, Style::default().fg(self.ui.accent)));
        parts.push(Span::styled(" | ", Style::default().fg(self.ui.muted)));
        parts.push(Span::styled(
            self.file_path
                .to_string_lossy()
                .to_string(),
            self.base_style,
        ));
        parts.push(Span::styled(" | ", Style::default().fg(self.ui.muted)));
        parts.push(Span::styled(
            format!("theme: {}", self.config.theme),
            Style::default().fg(self.ui.muted),
        ));
        if !self.search_query.is_empty() {
            let total = self.rendered.matches.len();
            let current = if total == 0 { 0 } else { self.current_match + 1 };
            parts.push(Span::styled(" | ", Style::default().fg(self.ui.muted)));
            parts.push(Span::styled(
                format!("search {current}/{total}"),
                Style::default().fg(self.ui.muted),
            ));
        }
        if let Some(msg) = &self.status {
            parts.push(Span::styled(" | ", Style::default().fg(self.ui.muted)));
            parts.push(Span::styled(msg.clone(), Style::default().fg(self.ui.accent)));
        }
        Line::from(parts)
    }

    fn edit_text(&self) -> Text<'static> {
        let selection = self.selection_range();
        let selected_style = self.base_style.add_modifier(Modifier::REVERSED);
        let mut lines = Vec::new();
        let mut char_index = 0usize;
        for line in self.rope.lines() {
            let mut s = line.to_string();
            if s.ends_with('\n') {
                s.pop();
                if s.ends_with('\r') {
                    s.pop();
                }
            }
            let line_visible_len = s.chars().count();
            let mut spans = Vec::new();
            if let Some((sel_start, sel_end, _)) = selection {
                let line_start = char_index;
                let line_end = line_start + line_visible_len;
                if sel_end <= line_start || sel_start >= line_end {
                    spans.push(Span::styled(s, self.base_style));
                } else {
                    let before_end = sel_start.saturating_sub(line_start).min(line_visible_len);
                    let after_start = sel_end.saturating_sub(line_start).min(line_visible_len);
                    if before_end > 0 {
                        spans.push(Span::styled(
                            slice_chars(&s, 0, before_end),
                            self.base_style,
                        ));
                    }
                    if after_start > before_end {
                        spans.push(Span::styled(
                            slice_chars(&s, before_end, after_start),
                            selected_style,
                        ));
                    }
                    if after_start < line_visible_len {
                        spans.push(Span::styled(
                            slice_chars(&s, after_start, line_visible_len),
                            self.base_style,
                        ));
                    }
                }
            } else {
                spans.push(Span::styled(s, self.base_style));
            }
            lines.push(Line::from(spans));
            char_index = char_index.saturating_add(line.len_chars());
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("", self.base_style)));
        }
        Text::from(lines)
    }

    fn ensure_editor_cache(&mut self) {
        if !self.editor_cache_dirty && !self.editor_lines.is_empty() {
            return;
        }
        self.editor_lines = self.build_editor_cache();
        self.editor_cache_dirty = false;
    }

    fn build_editor_cache(&self) -> Vec<Line<'static>> {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension("md")
            .or_else(|| self.syntax_set.find_syntax_by_token("Markdown"))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let theme = self.theme_manager.get(&self.config.theme);
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut lines = Vec::new();
        let mut in_code_block = false;
        let mut code_fence = String::new();
        let mut code_highlighter: Option<HighlightLines> = None;

        for line in self.rope.lines() {
            let line_str = line.to_string();
            let trimmed = line_str.trim_start();
            let fence = if trimmed.starts_with("```") {
                Some("```")
            } else if trimmed.starts_with("~~~") {
                Some("~~~")
            } else {
                None
            };

            if let Some(marker) = fence {
                if in_code_block && marker == code_fence {
                    in_code_block = false;
                    code_fence.clear();
                    code_highlighter = None;
                } else if !in_code_block {
                    in_code_block = true;
                    code_fence = marker.to_string();
                    let lang = trimmed[marker.len()..].trim();
                    let syntax = if lang.is_empty() {
                        self.syntax_set.find_syntax_plain_text()
                    } else {
                        self.syntax_set
                            .find_syntax_by_token(lang)
                            .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
                            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
                    };
                    code_highlighter = Some(HighlightLines::new(syntax, theme));
                }

                let line_widget = highlight_line_with(
                    &mut highlighter,
                    &self.syntax_set,
                    &line_str,
                    self.ui.base_bg,
                    self.base_style,
                );
                lines.push(line_widget);
                continue;
            }

            if in_code_block {
                if let Some(highlighter) = code_highlighter.as_mut() {
                    let line_widget = highlight_line_with(
                        highlighter,
                        &self.syntax_set,
                        &line_str,
                        self.ui.base_bg,
                        self.base_style,
                    );
                    lines.push(line_widget);
                } else {
                    lines.push(Line::from(Span::styled(
                        line_str.trim_end_matches('\n').to_string(),
                        self.base_style,
                    )));
                }
            } else {
                let line_widget = highlight_line_with(
                    &mut highlighter,
                    &self.syntax_set,
                    &line_str,
                    self.ui.base_bg,
                    self.base_style,
                );
                lines.push(line_widget);
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("", self.base_style)));
        }
        lines
    }

    fn editor_text(&mut self) -> Text<'static> {
        self.ensure_editor_cache();
        Text::from(self.editor_lines.clone())
    }

    fn cursor_screen_position(&self, layout: &LayoutInfo) -> Option<(u16, u16)> {
        if matches!(self.mode, Mode::CommandInput | Mode::SearchInput | Mode::ThemePicker) {
            return None;
        }
        self.edit_cursor_screen_position(layout)
    }

    fn edit_cursor_screen_position(&self, layout: &LayoutInfo) -> Option<(u16, u16)> {
        let (line, col) = self.cursor_line_col();
        if line < self.edit_scroll {
            return None;
        }
        let visible_line = line - self.edit_scroll;
        if visible_line >= layout.editor_height as usize {
            return None;
        }

        let mut line_str = self.rope.line(line).to_string();
        if line_str.ends_with('\n') {
            line_str.pop();
            if line_str.ends_with('\r') {
                line_str.pop();
            }
        }
        let mut width = 0usize;
        for ch in line_str.chars().take(col) {
            width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        let x = layout
            .editor
            .x
            .saturating_add(1)
            .saturating_add(width.min(layout.editor_width as usize).try_into().ok()?);
        let y = layout
            .editor
            .y
            .saturating_add(1)
            .saturating_add(visible_line.try_into().ok()?);
        Some((x, y))
    }
}

fn styles_from_palette(ui: UiPalette) -> (Style, MarkdownStyles) {
    let base_style = Style::default()
        .fg(ui.base_fg)
        .bg(bg_or_reset(ui.base_bg));

    let heading = Style::default()
        .fg(ui.accent)
        .add_modifier(Modifier::BOLD);
    let inline_code_bg = ui.code_bg.or_else(|| adjust_bg(ui.base_bg, -0.08));
    let inline_code = Style::default()
        .fg(ui.accent)
        .bg(bg_or_reset(inline_code_bg.or(ui.base_bg)));
    let prefix = Style::default().fg(ui.muted);
    let rule = Style::default().fg(ui.muted);

    (
        base_style,
        MarkdownStyles {
            base: base_style,
            heading,
            link_color: ui.accent,
            inline_code,
            prefix,
            rule,
            code_bg: inline_code_bg.or(ui.base_bg),
        },
    )
}

fn syntect_to_ratatui_style(
    style: syntect::highlighting::Style,
    base_bg: Option<Color>,
) -> Style {
    let mut out = Style::default()
        .fg(Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b));
    if let Some(bg) = base_bg {
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

fn highlight_line_with(
    highlighter: &mut HighlightLines,
    syntax_set: &SyntaxSet,
    line: &str,
    base_bg: Option<Color>,
    base_style: Style,
) -> Line<'static> {
    let ranges = match highlighter.highlight_line(line, syntax_set) {
        Ok(r) => r,
        Err(_) => vec![(syntect::highlighting::Style::default(), line)],
    };
    let mut spans = Vec::new();
    for (style, text) in ranges {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        spans.push(Span::styled(
            text.to_string(),
            syntect_to_ratatui_style(style, base_bg),
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled("", base_style));
    }
    Line::from(spans)
}

fn bg_or_reset(color: Option<Color>) -> Color {
    color.unwrap_or(Color::Reset)
}

fn adjust_bg(color: Option<Color>, delta: f32) -> Option<Color> {
    match color {
        Some(Color::Rgb(r, g, b)) => {
            let dr = adjust_channel(r, delta);
            let dg = adjust_channel(g, delta);
            let db = adjust_channel(b, delta);
            Some(Color::Rgb(dr, dg, db))
        }
        _ => None,
    }
}

fn adjust_channel(value: u8, delta: f32) -> u8 {
    let v = value as f32 / 255.0;
    let adjusted = (v + delta).clamp(0.0, 1.0);
    (adjusted * 255.0).round() as u8
}

fn line_len_chars(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let slice = rope.line(line);
    let mut len = slice.len_chars();
    if len == 0 {
        return 0;
    }
    if slice.char(len - 1) == '\n' {
        len = len.saturating_sub(1);
    }
    len
}

fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn normalize_markdown_line(line: &str) -> String {
    let trimmed = line.trim_end_matches(|c| c == '\n' || c == '\r');
    let stripped = strip_line_prefix(trimmed);
    let inline = strip_inline_markdown(&stripped);
    inline.trim().to_string()
}

fn normalize_prefix_for_col(line: &str, col: usize) -> usize {
    let prefix = slice_chars(line, 0, col);
    let stripped = strip_line_prefix(&prefix);
    let inline = strip_inline_markdown(&stripped);
    inline.chars().count()
}

fn build_match_candidates(line: &str, cursor_col: usize) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let len = trimmed.chars().count();
    let col = cursor_col.min(len);
    let mut out = Vec::new();

    if len > 0 {
        let start = col.saturating_sub(8);
        let end = (col + 8).min(len);
        let snippet = slice_chars(trimmed, start, end);
        if !snippet.trim().is_empty() {
            out.push(snippet);
        }
    }

    if let Some(word) = trimmed
        .split_whitespace()
        .max_by_key(|w| w.chars().count())
    {
        if word.chars().count() >= 4 {
            out.push(word.to_string());
        }
    }

    if len > 24 {
        out.push(slice_chars(trimmed, 0, 24));
    }

    out.push(trimmed.to_string());

    let mut unique = Vec::new();
    for cand in out {
        if !unique.iter().any(|v: &String| v == &cand) {
            unique.push(cand);
        }
    }
    unique
}

fn match_line(line: &str, candidates: &[String]) -> Option<(usize, Option<usize>)> {
    let mut best_len = 0usize;
    let mut best_col = None;
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        if let Some(byte_idx) = line.find(cand) {
            let len = cand.chars().count();
            let col = line[..byte_idx].chars().count();
            if len > best_len {
                best_len = len;
                best_col = Some(col);
            }
        }
    }
    if best_len > 0 {
        Some((best_len, best_col))
    } else {
        None
    }
}

fn strip_line_prefix(line: &str) -> String {
    let mut s = line.trim_start();
    let trimmed = s.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return String::new();
    }

    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix('>') {
            s = rest.trim_start();
            continue;
        }
        break;
    }

    let mut hash_count = 0usize;
    let mut hash_end = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch == '#' {
            hash_count += 1;
            hash_end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if hash_count > 0 && hash_count <= 6 {
        let rest = s[hash_end..].trim_start();
        let rest = rest.trim_end();
        return rest.trim_end_matches('#').trim_end().to_string();
    }

    if let Some(rest) = strip_list_marker(s) {
        return rest.to_string();
    }

    s.to_string()
}

fn strip_list_marker(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("- ") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("* ") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("+ ") {
        return Some(rest);
    }

    let mut end_digits = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch.is_ascii_digit() {
            end_digits = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end_digits > 0 {
        let rest = &s[end_digits..];
        if let Some(r) = rest.strip_prefix(". ") {
            return Some(r);
        }
        if let Some(r) = rest.strip_prefix(") ") {
            return Some(r);
        }
    }
    None
}

fn strip_inline_markdown(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '`' | '*' | '_' | '~' => {}
            '[' => {
                let mut text = String::new();
                while let Some(next) = chars.next() {
                    if next == ']' {
                        break;
                    }
                    text.push(next);
                }
                out.push_str(&text);
                if let Some('(') = chars.peek().copied() {
                    let _ = chars.next();
                    while let Some(next) = chars.next() {
                        if next == ')' {
                            break;
                        }
                    }
                }
            }
            '!' => {
                if let Some('[') = chars.peek().copied() {
                    let _ = chars.next();
                    let mut text = String::new();
                    while let Some(next) = chars.next() {
                        if next == ']' {
                            break;
                        }
                        text.push(next);
                    }
                    out.push_str(&text);
                    if let Some('(') = chars.peek().copied() {
                        let _ = chars.next();
                        while let Some(next) = chars.next() {
                            if next == ')' {
                                break;
                            }
                        }
                    }
                } else {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}
