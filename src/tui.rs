//! Interactive, read-only match browser. Source is tokenized on theme changes, never during drawing.

use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ra_ap_syntax::{AstNode, Edition, NodeOrToken, SourceFile, SyntaxKind};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::line::NORMAL,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::{
    snippet_diff::{LineChange, SnippetDiff},
    theme::{Theme, ThemeName},
    tui_data::{BrowserReport, Snippet},
};

type Highlighted = Vec<Line<'static>>;

/// Browse cached source context without reading files or modifying the report.
pub(crate) fn run(report: BrowserReport, editor: Option<&str>, theme: ThemeName) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "the match browser requires an interactive terminal on stdin and stdout; run without piping or redirection, or use a non-interactive output mode"
        );
    }
    let mut app = App::new(report, theme);

    // Restore before the normal panic diagnostic is printed. Catching the unwind lets
    // us put back the *original* hook outside an unwinding thread, even on setup failure.
    let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
    let saved = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        emergency_restore();
        if let Some(hook) = saved.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            hook(info);
        }
    }));
    let result = panic::catch_unwind(AssertUnwindSafe(|| run_terminal(&mut app, editor)));
    if let Some(hook) = previous.lock().unwrap_or_else(|e| e.into_inner()).take() {
        panic::set_hook(hook);
    }
    match result {
        Ok(result) => result,
        Err(payload) => panic::resume_unwind(payload),
    }
}

fn emergency_restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

#[derive(Default)]
struct Session {
    raw: bool,
    alternate: bool,
}

impl Session {
    fn enter(&mut self) -> Result<()> {
        // Mark attempts first: even a partially successful write must be undone.
        self.raw = true;
        enable_raw_mode().context("could not enable terminal raw mode")?;
        self.alternate = true;
        execute!(io::stdout(), EnterAlternateScreen)
            .context("could not enter the alternate terminal screen")?;
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        let screen = if self.alternate {
            execute!(io::stdout(), LeaveAlternateScreen, Show)
        } else {
            Ok(())
        };
        if screen.is_ok() {
            self.alternate = false;
        }
        let raw = if self.raw { disable_raw_mode() } else { Ok(()) };
        if raw.is_ok() {
            self.raw = false;
        }
        // Always attempt both restorations, even if the first fails.
        screen.context("could not restore the terminal screen")?;
        raw.context("could not restore terminal input")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

fn run_terminal(app: &mut App, editor: Option<&str>) -> Result<()> {
    let mut session = Session::default();
    session.enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        // Block until a meaningful event; no polling timer or repeated idle draws.
        let action = loop {
            match event::read().context("could not read terminal input")? {
                Event::Key(key) if key.kind != KeyEventKind::Release => break app.key(key),
                Event::Resize(_, _) => break Action::Draw,
                _ => {}
            }
        };
        match action {
            Action::Quit => break,
            Action::Draw => {}
            Action::Open => {
                if let Some(snippet) = app.snippet() {
                    let result = with_suspended(&mut session, || {
                        crate::editor::open(
                            &snippet.path,
                            snippet.start_line,
                            snippet.start_column,
                            editor,
                        )
                    })?;
                    // A terminal editor may have resized the window or overwritten
                    // the alternate screen. Invalidate ratatui's diff buffer.
                    terminal.clear()?;
                    app.status = Some(match result {
                        Ok(()) => (false, format!("Opened {}", snippet.path.display())),
                        Err(error) => (true, format!("Editor: {error:#}")),
                    });
                }
            }
        }
    }
    session.leave()
}

trait Suspend {
    fn suspend(&mut self) -> Result<()>;
    fn resume(&mut self) -> Result<()>;
}

impl Suspend for Session {
    fn suspend(&mut self) -> Result<()> {
        self.leave()
    }
    fn resume(&mut self) -> Result<()> {
        self.enter()
    }
}

fn with_suspended(
    session: &mut impl Suspend,
    open: impl FnOnce() -> Result<()>,
) -> Result<Result<()>> {
    session.suspend()?;
    let result = open();
    // Do not propagate an editor error until the UI has been restored.
    session
        .resume()
        .context("could not resume the browser after the editor")?;
    Ok(result)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Groups,
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Draw,
    Open,
    Quit,
}

struct PairDiff {
    group: usize,
    left: usize,
    right: usize,
    diff: SnippetDiff,
}

struct ThemePicker {
    original: ThemeName,
    original_highlighted: Vec<Vec<Arc<Highlighted>>>,
    query: String,
    visible: Vec<ThemeName>,
    list: ListState,
}

impl ThemePicker {
    fn new(current: ThemeName, highlighted: &[Vec<Arc<Highlighted>>]) -> Self {
        let visible = ThemeName::all().to_vec();
        let selected = visible.iter().position(|&name| name == current);
        Self {
            original: current,
            original_highlighted: highlighted.to_vec(),
            query: String::new(),
            visible,
            list: ListState::default().with_selected(selected),
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.visible = ThemeName::all()
            .iter()
            .copied()
            .filter(|name| {
                name.display_name().to_lowercase().contains(&query)
                    || name.slug().to_lowercase().contains(&query)
            })
            .collect();
        self.list = ListState::default().with_selected((!self.visible.is_empty()).then_some(0));
    }

    fn navigate(&mut self, forward: bool) {
        if let Some(selected) = self.list.selected() {
            self.list.select(Some(if forward {
                selected.saturating_add(1).min(self.visible.len() - 1)
            } else {
                selected.saturating_sub(1)
            }));
        }
    }

    fn key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down => self.navigate(true),
            KeyCode::Up => self.navigate(false),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate(true);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigate(false);
            }
            KeyCode::Home => self.list.select((!self.visible.is_empty()).then_some(0)),
            KeyCode::End => self.list.select(self.visible.len().checked_sub(1)),
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.filter();
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupSort {
    Occurrences,
    Lines,
    Files,
    Path,
}

impl GroupSort {
    const ALL: [Self; 4] = [Self::Occurrences, Self::Lines, Self::Files, Self::Path];

    fn label(self) -> &'static str {
        match self {
            Self::Occurrences => "occurrences ↓",
            Self::Lines => "matched lines ↓",
            Self::Files => "files ↓",
            Self::Path => "path ↑",
        }
    }
}

struct GroupMetadata {
    occurrences: usize,
    lines: usize,
    files: usize,
    path: PathBuf,
}

struct App {
    theme_name: ThemeName,
    theme_picker: Option<ThemePicker>,
    theme: Theme,
    diff_enabled: bool,
    pair_diff: Option<PairDiff>,
    report: BrowserReport,
    highlighted: Vec<Vec<Arc<Highlighted>>>,
    search_text: Vec<String>,
    metadata: Vec<GroupMetadata>,
    sort: GroupSort,
    sort_picker: Option<usize>,

    visible: Vec<usize>,
    selected: usize,
    occurrence: usize,
    scroll: usize,
    match_scroll_anchored: bool,
    horizontal: u16,
    cwd: Option<PathBuf>,
    page: usize,
    focus: Focus,
    query: String,
    searching: bool,
    help: bool,
    list: ListState,
    status: Option<(bool, String)>,
}

impl App {
    fn new(report: BrowserReport, theme_name: ThemeName) -> Self {
        let theme = Theme::new(theme_name);
        let highlighted = highlight_report(&report, &theme);
        let search_text = report
            .groups
            .iter()
            .map(|group| {
                let mut text = group.title.to_lowercase();
                for snippet in &group.occurrences {
                    text.push('\n');
                    text.push_str(&snippet.name.to_lowercase());
                    text.push('\n');
                    text.push_str(&snippet.path.to_string_lossy().to_lowercase());
                }
                text
            })
            .collect();
        let metadata = report
            .groups
            .iter()
            .map(|group| {
                let mut paths: Vec<_> = group.occurrences.iter().map(|s| s.path.clone()).collect();
                paths.sort();
                paths.dedup();
                GroupMetadata {
                    occurrences: group.occurrences.len(),
                    lines: group
                        .occurrences
                        .iter()
                        .map(|s| {
                            s.match_end_line
                                .saturating_sub(s.match_start_line)
                                .saturating_add(1)
                        })
                        .sum(),
                    files: paths.len(),
                    path: paths.first().cloned().unwrap_or_default(),
                }
            })
            .collect();
        let visible = (0..report.groups.len()).collect();
        let mut app = Self {
            theme_name,
            theme_picker: None,
            theme,
            diff_enabled: true,
            pair_diff: None,
            report,
            highlighted,
            search_text,
            metadata,
            sort: GroupSort::Occurrences,
            sort_picker: None,

            visible,
            selected: 0,
            occurrence: 0,
            scroll: 0,
            match_scroll_anchored: true,
            horizontal: 0,
            cwd: std::env::current_dir().ok(),
            page: 1,
            focus: Focus::Groups,
            query: String::new(),
            searching: false,
            help: false,
            list: ListState::default(),
            status: None,
        };
        app.sort_visible();
        app.reset_scroll();
        app.refresh_diff();
        app
    }

    fn sort_visible(&mut self) {
        let metadata = &self.metadata;
        self.visible.sort_by(|&a, &b| {
            let (left, right) = (&metadata[a], &metadata[b]);
            let order = match self.sort {
                GroupSort::Occurrences => right.occurrences.cmp(&left.occurrences),
                GroupSort::Lines => right.lines.cmp(&left.lines),
                GroupSort::Files => right.files.cmp(&left.files),
                GroupSort::Path => left.path.cmp(&right.path),
            };
            order
                .then_with(|| left.path.cmp(&right.path))
                .then(a.cmp(&b))
        });
    }

    fn apply_sort(&mut self, sort: GroupSort) {
        let old = self.group_index();
        self.sort = sort;
        self.sort_visible();
        self.selected = old
            .and_then(|g| self.visible.iter().position(|&v| v == g))
            .unwrap_or(0);
        self.list.select(old.map(|_| self.selected));
    }

    fn set_theme(&mut self, name: ThemeName) {
        if self.theme_name == name {
            return;
        }
        self.theme_name = name;
        self.theme = Theme::new(name);
        self.highlighted = match &self.theme_picker {
            Some(picker) if picker.original == name => picker.original_highlighted.clone(),
            _ => highlight_report(&self.report, &self.theme),
        };
    }

    // Cache only the displayed pair, never all O(n²) pairs of a large group.
    // Called on state transitions, not from draw (including resize redraws).
    fn refresh_diff(&mut self) {
        let pair = self.group_index().and_then(|g| {
            let count = self.report.groups[g].occurrences.len();
            (count > 1).then(|| (g, self.occurrence, (self.occurrence + 1) % count))
        });
        if self.pair_diff.as_ref().map(|p| (p.group, p.left, p.right)) == pair {
            return;
        }
        self.pair_diff = pair.map(|(group, left, right)| PairDiff {
            group,
            left,
            right,
            diff: SnippetDiff::between(
                &self.report.groups[group].occurrences[left].source,
                &self.report.groups[group].occurrences[right].source,
            ),
        });
    }

    fn group_index(&self) -> Option<usize> {
        self.visible.get(self.selected).copied()
    }

    fn snippet(&self) -> Option<&Snippet> {
        self.report
            .groups
            .get(self.group_index()?)?
            .occurrences
            .get(self.occurrence)
    }

    fn line_count(&self) -> usize {
        self.group_index()
            .and_then(|g| self.highlighted[g].get(self.occurrence))
            .map_or(0, |lines| lines.len())
    }

    fn max_scroll(&self) -> usize {
        self.line_count().saturating_sub(self.page)
    }

    fn reset_scroll(&mut self) {
        self.scroll = self.snippet().map_or(0, |s| initial_scroll(s, usize::MAX));
        self.match_scroll_anchored = true;
        self.horizontal = 0;
    }

    fn filter(&mut self) {
        let old = self.group_index();
        let query = self.query.to_lowercase();
        self.visible = self
            .search_text
            .iter()
            .enumerate()
            .filter_map(|(index, text)| text.contains(&query).then_some(index))
            .collect();
        self.sort_visible();
        self.selected = old
            .and_then(|g| self.visible.iter().position(|v| *v == g))
            .unwrap_or(0);
        self.occurrence = 0;
        self.reset_scroll();
        self.list = ListState::default();
        self.refresh_diff();
    }

    fn move_group(&mut self, forward: bool) {
        let next = if forward {
            self.selected
                .saturating_add(1)
                .min(self.visible.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
        if self.selected != next {
            self.selected = next;
            self.occurrence = 0;
            self.reset_scroll();
            self.refresh_diff();
        }
    }

    fn cycle(&mut self, forward: bool) {
        let count = self
            .group_index()
            .map_or(0, |g| self.report.groups[g].occurrences.len());
        if count > 0 {
            self.occurrence = if forward {
                (self.occurrence + 1) % count
            } else {
                (self.occurrence + count - 1) % count
            };
            self.reset_scroll();
            self.refresh_diff();
        }
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if let Some(picker) = &mut self.theme_picker {
            match key.code {
                KeyCode::Esc => {
                    let original = picker.original;
                    self.set_theme(original);
                    self.theme_picker = None;
                }
                KeyCode::Enter => {
                    if picker.list.selected().is_some() {
                        self.theme_picker = None;
                    }
                }
                _ => {
                    picker.key(key);
                    if let Some(name) = picker.list.selected().map(|i| picker.visible[i]) {
                        self.set_theme(name);
                    }
                }
            }
            return Action::Draw;
        }
        if let Some(selected) = self.sort_picker {
            match key.code {
                KeyCode::Esc => self.sort_picker = None,
                KeyCode::Enter => {
                    self.apply_sort(GroupSort::ALL[selected]);
                    self.sort_picker = None;
                }
                KeyCode::Down => self.sort_picker = Some((selected + 1).min(3)),
                KeyCode::Up => self.sort_picker = Some(selected.saturating_sub(1)),
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.sort_picker = Some((selected + 1).min(3));
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.sort_picker = Some(selected.saturating_sub(1));
                }
                _ => {}
            }
            return Action::Draw;
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                self.help = false;
            } else if key.code == KeyCode::Char('q') {
                return Action::Quit;
            }
            return Action::Draw;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.filter();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.filter();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(c);
                    self.filter();
                }
                _ => {}
            }
            return Action::Draw;
        }
        if self.focus == Focus::Code
            && matches!(
                key.code,
                KeyCode::Down
                    | KeyCode::Up
                    | KeyCode::PageDown
                    | KeyCode::PageUp
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Char('j' | 'k' | 'g' | 'G')
            )
        {
            self.match_scroll_anchored = false;
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('s') => {
                self.sort_picker = GroupSort::ALL.iter().position(|&s| s == self.sort)
            }

            KeyCode::Char('d') => self.diff_enabled = !self.diff_enabled,
            KeyCode::Char('t') => {
                self.theme_picker = Some(ThemePicker::new(self.theme_name, &self.highlighted));
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Esc => {
                self.query.clear();
                self.filter();
                self.status = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Code,
                    Focus::Code => Focus::Groups,
                }
            }
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.focus == Focus::Code {
                    self.horizontal = if key.code == KeyCode::Right {
                        self.horizontal.saturating_add(4)
                    } else {
                        self.horizontal.saturating_sub(4)
                    };
                }
            }
            KeyCode::Char('H' | 'L') => {
                if self.focus == Focus::Code {
                    self.horizontal = if key.code == KeyCode::Char('L') {
                        self.horizontal.saturating_add(4)
                    } else {
                        self.horizontal.saturating_sub(4)
                    };
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Groups,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Code,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Groups {
                    self.move_group(true);
                } else {
                    self.scroll = self.scroll.saturating_add(1).min(self.max_scroll());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Groups {
                    self.move_group(false);
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::Char('n') | KeyCode::Char(']') => self.cycle(true),
            KeyCode::Char('p') | KeyCode::Char('[') => self.cycle(false),
            KeyCode::PageDown => {
                if self.focus == Focus::Groups {
                    self.selected = self
                        .selected
                        .saturating_add(self.page)
                        .min(self.visible.len().saturating_sub(1));
                    self.occurrence = 0;
                    self.reset_scroll();
                } else {
                    self.scroll = self.scroll.saturating_add(self.page).min(self.max_scroll());
                }
            }
            KeyCode::PageUp => {
                if self.focus == Focus::Groups {
                    self.selected = self.selected.saturating_sub(self.page);
                    self.occurrence = 0;
                    self.reset_scroll();
                } else {
                    self.scroll = self.scroll.saturating_sub(self.page);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if self.focus == Focus::Groups {
                    self.selected = 0;
                    self.occurrence = 0;
                    self.reset_scroll();
                } else {
                    self.scroll = 0;
                    self.horizontal = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if self.focus == Focus::Groups {
                    self.selected = self.visible.len().saturating_sub(1);
                    self.occurrence = 0;
                    self.reset_scroll();
                } else {
                    self.scroll = self.max_scroll();
                }
            }
            KeyCode::Enter | KeyCode::Char('o') if self.snippet().is_some() => {
                return Action::Open;
            }
            _ => {}
        }
        self.refresh_diff();
        Action::Draw
    }
}

fn initial_scroll(snippet: &Snippet, page: usize) -> usize {
    snippet
        .match_start_line
        .saturating_sub(snippet.display_start_line)
        .saturating_sub(3.min(page.saturating_sub(1)))
}

fn highlight_report(report: &BrowserReport, theme: &Theme) -> Vec<Vec<Arc<Highlighted>>> {
    let mut cache: HashMap<Arc<str>, Arc<Highlighted>> = HashMap::new();
    report
        .groups
        .iter()
        .map(|group| {
            group
                .occurrences
                .iter()
                .map(|snippet| {
                    Arc::clone(
                        cache
                            .entry(Arc::clone(&snippet.display_source))
                            .or_insert_with(|| Arc::new(highlight(&snippet.display_source, theme))),
                    )
                })
                .collect()
        })
        .collect()
}

fn highlight(source: &str, theme: &Theme) -> Highlighted {
    let parsed = SourceFile::parse(source, Edition::CURRENT);
    let tokens: Vec<_> = parsed
        .tree()
        .syntax()
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .collect();
    let mut lines = vec![Line::default()];
    for (index, token) in tokens.iter().enumerate() {
        let kind = token.kind();
        let color = if kind == SyntaxKind::COMMENT {
            theme.comment
        } else if matches!(
            kind,
            SyntaxKind::STRING
                | SyntaxKind::BYTE_STRING
                | SyntaxKind::C_STRING
                | SyntaxKind::CHAR
                | SyntaxKind::BYTE
        ) {
            theme.string
        } else if matches!(kind, SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER) {
            theme.number
        } else if kind.is_keyword(Edition::CURRENT) {
            theme.keyword
        } else if kind == SyntaxKind::IDENT
            && tokens[index + 1..]
                .iter()
                .find(|next| !matches!(next.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
                .is_some_and(|next| next.text() == "(")
        {
            theme.function
        } else {
            theme.foreground
        };
        // Splitting *after* lexing preserves multiline raw strings and comments.
        for (part_index, part) in token.text().split('\n').enumerate() {
            if part_index > 0 {
                lines.push(Line::default());
            }
            if !part.is_empty() {
                // Tabs have no portable terminal width; use a consistent four spaces.
                let text = part.replace('\t', "    ").replace('\r', "");
                if let Some(line) = lines.last_mut() {
                    line.spans
                        .push(Span::styled(text, Style::default().fg(color)));
                }
            }
        }
    }
    if source.ends_with('\n') && lines.len() > 1 {
        lines.pop();
    }
    lines
}

fn panel(theme: &Theme, title: impl Into<Line<'static>>, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if active { theme.accent } else { theme.border })
        .style(theme.panel)
}

fn draw(frame: &mut Frame, app: &mut App) {
    // Browser layout may clamp scroll and update list/page state on resize, but
    // those changes must not leak out while the picker owns input.
    let saved = (app.theme_picker.is_some() || app.sort_picker.is_some())
        .then_some((app.scroll, app.page, app.list));
    draw_browser(frame, app);
    if let Some((scroll, page, list)) = saved {
        (app.scroll, app.page, app.list) = (scroll, page, list);
        for cell in &mut frame.buffer_mut().content {
            cell.fg = dim_foreground(cell.fg, cell.bg);
        }
        let area = frame.area();
        if let Some(picker) = &mut app.theme_picker {
            draw_theme_picker(frame, picker, &app.theme, area);
        } else if let Some(selected) = app.sort_picker {
            draw_sort_picker(frame, selected, &app.theme, area);
        }
    }
}

fn dim_foreground(fg: Color, bg: Color) -> Color {
    match (fg, bg) {
        (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
            let blend = |fg: u8, bg: u8| ((u16::from(fg) * 3 + u16::from(bg)) / 4) as u8;
            Color::Rgb(blend(r, br), blend(g, bg), blend(b, bb))
        }
        _ => fg,
    }
}

fn draw_browser(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(app.theme.base), area);
    if area.width < 60 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("rsdup · match browser", app.theme.accent),
                Line::from("Enlarge terminal to at least 60 × 12."),
                Line::from(format!(
                    "{} matches · {} file{}",
                    app.report.groups.len(),
                    app.report.files,
                    if app.report.files == 1 { "" } else { "s" }
                )),
                Line::from(format!(
                    "{} · t choose theme",
                    app.theme_name.display_name()
                )),
                Line::from("q / Ctrl-C quit · ? help"),
            ]),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" rsdup ", app.theme.accent.add_modifier(Modifier::BOLD)),
                Span::raw(format!(
                    "/ {} / match browser · {}",
                    app.report.mode,
                    app.theme_name.display_name()
                )),
            ]),
            Line::styled(
                format!(
                    " {} matches · {} file{} · read-only",
                    app.report.groups.len(),
                    app.report.files,
                    if app.report.files == 1 { "" } else { "s" }
                ),
                app.theme.muted,
            ),
        ]),
        rows[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(area.width)),
            Constraint::Min(1),
        ])
        .split(rows[1]);
    draw_groups(frame, app, columns[0]);
    draw_code(frame, app, columns[1]);
    draw_footer(frame, app, rows[2]);
    if app.help {
        draw_help(frame, &app.theme, area);
    }
}

fn sidebar_width(width: u16) -> u16 {
    (u32::from(width) * 2 / 5).clamp(24, 80) as u16
}

// Use the renderer's grapheme boundaries and cell widths, including combining marks.
fn compact_line(line: Line<'static>, width: u16) -> Line<'static> {
    if line.width() <= usize::from(width) {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let mut result = Line::default();
    let mut used = 0;
    for span in &line.spans {
        for grapheme in span.styled_graphemes(line.style) {
            let size = Line::from(grapheme.symbol).width();
            if used + size >= usize::from(width) {
                result.spans.push(Span::styled("…", grapheme.style));
                return result;
            }
            result
                .spans
                .push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            used += size;
        }
    }
    result
}

// Discard parents first; retain the filename (or its suffix) and its styling.
fn compact_location(line: Line<'static>, width: u16) -> Line<'static> {
    if line.width() <= usize::from(width) {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let graphemes: Vec<_> = line
        .spans
        .iter()
        .flat_map(|span| span.styled_graphemes(line.style))
        .collect();
    let mut used = 1;
    let mut suffix = Vec::new();
    for grapheme in graphemes.iter().rev() {
        let size = Line::from(grapheme.symbol).width();
        if used + size > usize::from(width) {
            break;
        }
        used += size;
        suffix.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
    }
    suffix.push(Span::styled(
        "…",
        line.spans
            .first()
            .map_or(line.style, |span| line.style.patch(span.style)),
    ));
    suffix.reverse();
    Line::from(suffix)
}

fn browser_path<'a>(app: &App, path: &'a Path) -> &'a Path {
    app.cwd
        .as_deref()
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path)
}

// Compare path components, not string prefixes (src/foo is not src/foobar).
fn shared_parent<'a>(paths: &[&'a Path]) -> &'a Path {
    let mut parent = paths
        .first()
        .and_then(|path| path.parent())
        .unwrap_or(Path::new(""));
    for path in paths.iter().skip(1) {
        let directory = path.parent().unwrap_or(Path::new(""));
        while !directory.starts_with(parent) {
            match parent.parent() {
                Some(next) => parent = next,
                None => return Path::new(""),
            }
        }
    }
    parent
}

fn compact_context(text: String, style: Style, width: u16) -> Line<'static> {
    let line = Line::styled(text, style);
    if line.width() <= usize::from(width) || width < 3 {
        return compact_line(line, width);
    }
    let mut prefix = compact_line(line.clone(), width / 2);
    // Both halves elide; keep only one ellipsis in the middle.
    prefix.spans.pop();
    let remaining = width - prefix.width() as u16;
    prefix.spans.extend(compact_location(line, remaining).spans);
    prefix
}

fn group_card(app: &App, index: usize, width: u16) -> Vec<Line<'static>> {
    let meta = &app.metadata[index];
    let mut summary = format!(
        "{} occurrence{}",
        meta.occurrences,
        if meta.occurrences == 1 { "" } else { "s" }
    );
    for (count, noun) in [(meta.lines, "line"), (meta.files, "file")] {
        let extra = format!(" · {count} {noun}{}", if count == 1 { "" } else { "s" });
        if Line::from(format!("{summary}{extra}")).width() <= usize::from(width) {
            summary.push_str(&extra);
        }
    }
    let mut lines = vec![compact_line(Line::styled(summary, app.theme.accent), width)];
    let occurrences = &app.report.groups[index].occurrences;
    let paths: Vec<_> = occurrences
        .iter()
        .map(|s| browser_path(app, &s.path))
        .collect();
    let parent = shared_parent(&paths);
    if !parent.as_os_str().is_empty() {
        lines.push(compact_context(
            parent.to_string_lossy().into_owned(),
            app.theme.muted,
            width,
        ));
    }
    let selected = app.group_index() == Some(index);
    let start = if selected { app.occurrence / 2 * 2 } else { 0 };
    let end = (start + 2).min(occurrences.len());
    let mut files: Vec<(&Path, Vec<usize>)> = Vec::new();
    for (i, &path) in paths.iter().enumerate().take(end).skip(start) {
        if let Some((_, indices)) = files.iter_mut().find(|(file, _)| *file == path) {
            indices.push(i);
        } else {
            files.push((path, vec![i]));
        }
    }
    let details_start = lines.len();
    for (file_index, (path, indices)) in files.iter().enumerate() {
        let last = file_index + 1 == files.len();
        let branch = if last {
            NORMAL.bottom_left
        } else {
            NORMAL.vertical_right
        };
        let suffix = path.strip_prefix(parent).unwrap_or(path).to_string_lossy();
        let mut heading = Line::styled(format!("{branch}{} ", NORMAL.horizontal), app.theme.muted);
        heading.spans.extend(
            compact_location(
                Line::from(Span::styled(
                    suffix.into_owned(),
                    Style::default().fg(app.theme.foreground),
                )),
                width.saturating_sub(3),
            )
            .spans,
        );
        lines.push(compact_line(heading, width));
        for &i in indices {
            let snippet = &occurrences[i];
            let mut row = Line::from(vec![
                Span::styled(
                    format!("{}  ", if last { " " } else { NORMAL.vertical }),
                    app.theme.muted,
                ),
                Span::styled(
                    if selected && i == app.occurrence {
                        "▸ "
                    } else {
                        "  "
                    },
                    app.theme.muted,
                ),
            ]);
            // Preserve the function first; line numbers use only the space left over.
            row.spans.extend(
                compact_line(
                    Line::from(Span::styled(
                        snippet.name.clone(),
                        Style::default().fg(app.theme.function),
                    )),
                    width.saturating_sub(5),
                )
                .spans,
            );
            let remaining = width.saturating_sub(row.width() as u16);
            row.spans.extend(
                compact_line(
                    Line::from(Span::styled(
                        format!(":{}", snippet.start_line),
                        app.theme.muted,
                    )),
                    remaining,
                )
                .spans,
            );
            lines.push(compact_line(row, width));
        }
    }
    // Reserve the largest stable pair, including an odd final page. Selection and
    // n/p must never move the following cards, even when a pair shares a file.
    let header_rows = paths
        .chunks(2)
        .map(|pair| {
            if pair.len() == 2 && pair[0] != pair[1] {
                2
            } else {
                1
            }
        })
        .max()
        .unwrap_or(0);
    lines.resize(
        details_start + occurrences.len().min(2) + header_rows,
        Line::default(),
    );
    if occurrences.len() > 2 {
        lines.push(compact_line(
            Line::styled(
                format!("+{} occurrences · n/p", occurrences.len() - (end - start)),
                app.theme.muted,
            ),
            width,
        ));
    }
    lines.push(Line::default());
    lines
}

fn group_selection(theme: &Theme) -> Style {
    // Keep semantic foregrounds intact: a quarter-strength selection tint avoids
    // the saturated selection colors that some palettes also use for text.
    let bg = match (theme.panel.bg, theme.selection.bg) {
        (Some(panel @ Color::Rgb(..)), Some(selection @ Color::Rgb(..))) => {
            Some(dim_foreground(panel, selection))
        }
        (_, selection) => selection,
    };
    Style {
        bg,
        ..Style::default()
    }
}

fn draw_groups(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(3);

    let items: Vec<_> = app
        .visible
        .iter()
        .map(|&index| ListItem::new(group_card(app, index, width)))
        .collect();
    app.list
        .select((!app.visible.is_empty()).then_some(app.selected));
    let block = panel(
        &app.theme,
        format!(
            " {} · {}/{} ",
            app.sort.label(),
            app.visible.len(),
            app.report.groups.len()
        ),
        app.focus == Focus::Groups,
    );
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.query.is_empty() {
                "No matches in this report."
            } else {
                "No matches for this search.\nEsc clears the filter."
            })
            .block(block),
            area,
        );
    } else {
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_style(group_selection(&app.theme))
                .highlight_symbol("▸"),
            area,
            &mut app.list,
        );
        // Wide-grapheme continuation cells are reset by the text renderer.
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let buffer = frame.buffer_mut();
                if buffer[(x, y)].fg == Color::Reset && x > area.x {
                    let style = buffer[(x - 1, y)].style();
                    buffer[(x, y)].set_style(style);
                }
            }
        }
    }
}

fn draw_code(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(group_index) = app.group_index() else {
        frame.render_widget(
            Paragraph::new("Select a match, or press / to search.")
                .block(panel(&app.theme, " Source ", false)),
            area,
        );
        return;
    };
    let group = &app.report.groups[group_index];
    let count = group.occurrences.len();
    if count == 0 {
        frame.render_widget(
            Paragraph::new("This group has no occurrences.")
                .block(panel(&app.theme, " Source ", false)),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    " Match #{} · occurrence {}/{}",
                    group_index + 1,
                    app.occurrence + 1,
                    count
                ),
                app.theme.accent,
            ),
            Line::styled(format!(" {}", group.description), app.theme.muted),
        ]),
        rows[0],
    );
    let pair = count > 1 && rows[1].height >= 18;
    let cards = if pair {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1])
            .to_vec()
    } else {
        vec![rows[1]]
    };
    // Four metadata rows (path, evidence, warning/context, plus borders) leave
    // the rest for source. Clamp after every resize, before rendering the source.
    app.page = usize::from(cards[0].height.saturating_sub(5)).max(1);
    // Keep a newly selected match visible across resizes until the user scrolls.
    if app.match_scroll_anchored {
        app.scroll = app.snippet().map_or(0, |s| initial_scroll(s, app.page));
    }
    app.scroll = app.scroll.min(app.max_scroll());
    for (position, card) in cards.into_iter().enumerate() {
        let occurrence = (app.occurrence + position) % count;
        draw_card(frame, app, group_index, occurrence, position == 0, card);
    }
}

fn evidence(app: &App, group_index: usize, _occurrence: usize) -> String {
    let group = &app.report.groups[group_index];
    if group.occurrences.len() < 2 {
        return "No comparison pair".into();
    }
    let left = app.occurrence;
    let right = (left + 1) % group.occurrences.len();
    let pair = format!("#{} ↔ #{}", left + 1, right + 1);
    if let Some(edge) = group
        .edges
        .iter()
        .find(|e| (e.left == left && e.right == right) || (e.left == right && e.right == left))
    {
        format!(
            "{pair}: {}cosine {:.3}",
            if edge.exact_body {
                "same-token · "
            } else {
                ""
            },
            edge.cosine
        )
    } else {
        format!("No pair similarity: {pair} · no direct edge")
    }
}

/// Display-only shortening: editor launches always retain the original path.
fn display_location(snippet: &Snippet, cwd: Option<&Path>, width: u16) -> String {
    let path = cwd
        .and_then(|cwd| snippet.path.strip_prefix(cwd).ok())
        .unwrap_or(&snippet.path);
    let location = format!(
        "{}:{}:{}",
        path.display(),
        snippet.start_line,
        snippet.start_column
    );
    if Line::from(location.as_str()).width() <= usize::from(width) {
        return location;
    }
    if width == 0 {
        return String::new();
    }
    // Keep the filename and position at the right, discarding parents first.
    // Even unusually long filenames keep their suffix; never split a grapheme.
    let span = Span::raw(location.as_str());
    let graphemes: Vec<_> = span.styled_graphemes(Style::default()).collect();
    let mut used = 1;
    let mut start = graphemes.len();
    for (index, grapheme) in graphemes.iter().enumerate().rev() {
        let next = Line::from(grapheme.symbol).width();
        if used + next > usize::from(width) {
            break;
        }
        used += next;
        start = index;
    }
    let mut result = String::from("…");
    for grapheme in &graphemes[start..] {
        result.push_str(grapheme.symbol);
    }
    result
}

fn draw_card(
    frame: &mut Frame,
    app: &App,
    group: usize,
    occurrence: usize,
    focused: bool,
    area: Rect,
) {
    let snippet = &app.report.groups[group].occurrences[occurrence];
    let lines = &app.highlighted[group][occurrence];
    let page = usize::from(area.height.saturating_sub(5)).max(1);
    let scroll = if focused {
        app.scroll
    } else {
        initial_scroll(snippet, page).min(lines.len().saturating_sub(page))
    };
    let block = panel(
        &app.theme,
        format!(
            " {}#{} {} ",
            if focused { "▸ " } else { "" },
            occurrence + 1,
            snippet.name
        ),
        focused && app.focus == Focus::Code,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(display_location(snippet, app.cwd.as_deref(), rows[0].width))
            .style(app.theme.location),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(evidence(app, group, occurrence)).style(app.theme.accent),
        rows[1],
    );
    let context = if snippet.test_context {
        "test context"
    } else {
        "source"
    };
    let info = if let Some(warning) = &snippet.warning {
        format!("WARNING: {warning}")
    } else {
        format!(
            "context dimmed · matched lines {}–{} · {context}",
            snippet.match_start_line, snippet.match_end_line
        )
    };
    frame.render_widget(
        Paragraph::new(info).style(if snippet.warning.is_some() {
            app.theme.warning
        } else {
            app.theme.muted
        }),
        rows[2],
    );
    let digits = snippet
        .display_start_line
        .saturating_add(lines.len().saturating_sub(1))
        .to_string()
        .len()
        .max(3);
    let gutter_width = (digits + 3).min(usize::from(rows[3].width)) as u16;
    let gutter = Rect::new(rows[3].x, rows[3].y, gutter_width, rows[3].height);
    let code = Rect::new(
        rows[3].x.saturating_add(gutter_width),
        rows[3].y,
        rows[3].width.saturating_sub(gutter_width),
        rows[3].height,
    );
    let visible = lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(code.height));
    let changes = app
        .pair_diff
        .as_ref()
        .filter(|p| app.diff_enabled && p.group == group)
        .and_then(|p| {
            if occurrence == p.left {
                Some((&p.diff.left, '−'))
            } else if occurrence == p.right {
                Some((&p.diff.right, '+'))
            } else {
                None
            }
        });
    let match_index = |index: usize| {
        let line = snippet.display_start_line.saturating_add(index);
        (snippet.match_start_line..=snippet.match_end_line)
            .contains(&line)
            .then(|| line - snippet.match_start_line)
    };
    let decoration = |index: usize| {
        let change = match_index(index)
            .and_then(|relative| changes.and_then(|(lines, _)| lines.get(relative)))
            .copied()
            .unwrap_or(LineChange::Equal);
        match change {
            LineChange::Equal => (
                '│',
                Style {
                    bg: app.theme.panel.bg,
                    ..Style::default()
                },
                app.theme.gutter,
            ),
            LineChange::Removed => ('−', app.theme.removed.row, app.theme.removed.gutter),
            LineChange::Added => ('+', app.theme.added.row, app.theme.added.gutter),
            LineChange::Changed => (
                changes.map_or('│', |(_, marker)| marker),
                app.theme.changed.row,
                app.theme.changed.gutter,
            ),
        }
    };
    let numbers: Vec<_> = visible
        .clone()
        .map(|(index, _)| {
            let (marker, _, gutter_style) = decoration(index);
            Line::styled(
                format!(
                    "{:>digits$} {marker} ",
                    snippet.display_start_line.saturating_add(index)
                ),
                gutter_style,
            )
        })
        .collect();
    let source: Vec<_> = visible
        .map(|(index, line)| {
            let mut line = line.clone();
            line.style = decoration(index).1;
            // Context is line-based: boundary lines retain syntax even when the
            // matched item starts or ends mid-line. Override token colors too.
            if match_index(index).is_none() {
                line.style = app.theme.context;
                for span in &mut line.spans {
                    span.style = app.theme.context;
                }
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(numbers).style(app.theme.gutter), gutter);
    // Ratatui scrolls styled graphemes in terminal columns, not UTF-8 bytes.
    // Keeping the gutter separate preserves line numbers at every offset.
    frame.render_widget(
        Paragraph::new(source).scroll((0, if focused { app.horizontal } else { 0 })),
        code,
    );
    // Paragraph resets continuation cells of wide graphemes. Apply row colors
    // after rendering so context (and diff backgrounds) also cover those cells.
    for (row, index) in (scroll..lines.len())
        .take(usize::from(code.height))
        .enumerate()
    {
        let mut style = decoration(index).1;
        if match_index(index).is_none() {
            style = app.theme.context;
        }
        frame
            .buffer_mut()
            .set_style(Rect::new(code.x, code.y + row as u16, code.width, 1), style);
    }
}

fn diff_legend(app: &App) -> String {
    let Some(pair) = &app.pair_diff else {
        return " No comparison pair · d diff · context dimmed".into();
    };
    let a = pair.left + 1;
    let b = pair.right + 1;
    format!(
        " {}compared #{a} ↔ #{b}; − only #{a} / + only #{b}{} · context dimmed",
        if pair.diff.limited { "LIMIT: " } else { "" },
        if app.diff_enabled {
            " · d on"
        } else {
            " · d OFF"
        }
    )
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let message = if app.searching {
        format!(" /{}▏  · Enter keep · Esc clear/cancel", app.query)
    } else if let Some((_, status)) = &app.status {
        format!(" {status}")
    } else if !app.query.is_empty() {
        format!(" Filter: {} · Esc clear", app.query)
    } else {
        format!(
            " {} focus · match {}/{} · occurrence {}",
            if app.focus == Focus::Groups {
                "Matches"
            } else {
                "Source"
            },
            if app.visible.is_empty() {
                0
            } else {
                app.selected + 1
            },
            app.visible.len(),
            if app.snippet().is_some() {
                app.occurrence + 1
            } else {
                0
            }
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                message,
                if app.status.as_ref().is_some_and(|(error, _)| *error) && !app.searching {
                    app.theme.error
                } else {
                    app.theme.accent
                },
            ),
            Line::from(diff_legend(app)),
            Line::styled(
                format!(
                    " s sort · t theme: {} · n/p pair · ? help · q quit",
                    app.theme_name.display_name()
                ),
                app.theme.muted,
            ),
        ]),
        area,
    );
}

fn draw_sort_picker(frame: &mut Frame, selected: usize, theme: &Theme, area: Rect) {
    let width = area.width.min(48);
    let height = area.height.min(8);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = panel(theme, " Sort · Enter apply / Esc cancel ", true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(GroupSort::ALL.map(|sort| ListItem::new(sort.label())))
            .style(theme.panel)
            .highlight_style(theme.selection)
            .highlight_symbol("▸ "),
        rows[0],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("↑/↓ or Ctrl-N/P · lines = sum, not savings").style(theme.muted),
        rows[1],
    );
}

fn draw_theme_picker(frame: &mut Frame, picker: &mut ThemePicker, theme: &Theme, area: Rect) {
    let width = area.width.min(72);
    let height = area.height.min(20);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(theme.panel), popup);
    let inner = if width >= 4 && height >= 4 {
        let block = panel(theme, " Choose theme ", true);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        inner
    } else {
        popup
    };
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(u16::from(inner.height >= 4)),
        Constraint::Length(u16::from(inner.height >= 5)),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!("Filter: {}▏", picker.query)).style(theme.accent),
        rows[0],
    );
    if picker.visible.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching themes").style(theme.muted),
            rows[1],
        );
    } else {
        let items = picker.visible.iter().map(|name| {
            ListItem::new(format!(
                "{}{} ({})",
                if *name == picker.original { "* " } else { "  " },
                name.display_name(),
                name.slug(),
            ))
        });
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(theme.selection)
                .highlight_symbol("▸"),
            rows[1],
            &mut picker.list,
        );
    }
    frame.render_widget(
        Paragraph::new("Type to filter · ↑/↓ or Ctrl-N/P move · * original").style(theme.muted),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new("Enter apply · Esc cancel · Ctrl-C quit").style(theme.muted),
        rows[3],
    );
}

fn draw_help(frame: &mut Frame, theme: &Theme, area: Rect) {
    let width = area.width.min(68);
    let height = area.height.min(24);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Tab / Shift-Tab     Switch matches / source focus"),
            Line::from("h / ←, l / →        Focus matches, focus source"),
            Line::from("j/k / ↓↑            Select match or scroll source context"),
            Line::from("H/L / Shift-←→      Pan source by 4 columns (code focus)"),
            Line::from("n/p / ] [           Next / previous occurrence (wraps)"),
            Line::from("d                   Toggle exact-text diff (default on)"),
            Line::from("t                   Choose theme; type to filter"),
            Line::from("s                   Sort groups (Enter apply / Esc cancel)"),
            Line::from("PgUp / PgDn         Page in the focused pane"),
            Line::from("g/G / Home/End      First / last match or source page"),
            Line::from("Context dimmed by line; matched lines keep syntax and diff."),
            Line::from("/                   Search titles, names and paths"),
            Line::from("Enter in search     Keep filter; return to navigation"),
            Line::from("Esc                 Clear filter / cancel search"),
            Line::from("Enter / o           Open focused occurrence in editor"),
            Line::from("Picker: ↑/↓ or Ctrl-N/P move; Enter apply; Esc cancel"),
            Line::from("q / Ctrl-C          Quit (Ctrl-C works in every mode)"),
            Line::from("±: changed; LIMIT: conservative marks (work cap)"),
            Line::styled(
                "Pair evidence is not a semantic equivalence claim.",
                theme.muted,
            ),
            Line::from("Esc / ? / Enter     Close help"),
        ])
        .block(panel(theme, " Browser controls ", true)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_data::{MatchGroup, PairEvidence};
    use ratatui::{backend::TestBackend, style::Color};
    use std::path::PathBuf;

    fn dracula() -> Theme {
        Theme::new(ThemeName::Dracula)
    }

    fn dracula_diff_background(bg: Color) -> bool {
        [
            dracula().added.row.bg,
            dracula().removed.row.bg,
            dracula().changed.row.bg,
        ]
        .contains(&Some(bg))
    }

    fn assert_readable(buffer: &ratatui::buffer::Buffer, name: ThemeName) {
        let palette = name.palette();
        let theme = Theme::new(name);
        let foregrounds = [
            palette.fg,
            palette.muted,
            palette.accent,
            palette.secondary,
            palette.error,
            palette.warning,
            palette.success,
            palette.info,
        ];
        let backgrounds = [
            palette.bg,
            palette.selection,
            group_selection(&theme).bg.unwrap(),
            theme.removed.row.bg.unwrap(),
            theme.added.row.bg.unwrap(),
            theme.changed.row.bg.unwrap(),
        ];
        for cell in &buffer.content {
            assert!(foregrounds.contains(&cell.fg), "{name:?}: {cell:?}");
            assert!(backgrounds.contains(&cell.bg), "{name:?}: {cell:?}");
            if !cell.symbol().trim().is_empty() {
                assert_ne!(cell.fg, cell.bg, "{name:?}: {cell:?}");
            }
        }
    }

    #[test]
    fn themes_cover_source_context_diff_selection_help_and_small_screens() {
        for &name in ThemeName::all() {
            let mut app = App::new(context_report(), name);
            let cache = Arc::clone(&app.highlighted[0][0]);
            for (width, height) in [(1, 1), (20, 6), (59, 20), (60, 12), (100, 24), (160, 48)] {
                for help in [false, true] {
                    app.help = help;
                    assert_readable(&render(&mut app, width, height).1, name);
                }
            }
            app.help = false;
            app.focus = Focus::Code;
            app.horizontal = 4;
            assert_readable(&render(&mut app, 100, 24).1, name);
            press(&mut app, KeyCode::Char('d'));
            assert_readable(&render(&mut app, 160, 48).1, name);
            assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
            app.status = Some((true, "Editor failed".into()));
            app.report.groups[0].occurrences[0].warning = Some("source missing".into());
            assert_readable(&render(&mut app, 100, 24).1, name);
            app.searching = true;
            app.query = "missing".into();
            app.filter();
            assert_readable(&render(&mut app, 100, 24).1, name);
            let mut empty_report = context_report();
            empty_report.groups.clear();
            let mut app = App::new(empty_report, name);
            let (text, buffer) = render(&mut app, 100, 24);
            assert!(text.contains("No matches in"));
            assert_readable(&buffer, name);
        }
    }

    #[test]
    fn picking_themes_preserves_navigation_diff_filter_and_shared_context() {
        let mut app = App::new(context_report(), ThemeName::default());
        app.query = "parsers".into();
        app.filter();
        render(&mut app, 160, 48);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('d'));
        app.status = Some((false, "unchanged".into()));
        let state = (
            app.visible.clone(),
            app.selected,
            app.occurrence,
            app.scroll,
            app.match_scroll_anchored,
            app.horizontal,
            app.focus,
            app.query.clone(),
            app.diff_enabled,
            app.list,
            app.status.clone(),
        );
        let pair = app.pair_diff.as_ref().unwrap().diff.left.as_ptr();
        let original = Arc::clone(&app.highlighted[0][0]);
        for &expected in ThemeName::all() {
            press(&mut app, KeyCode::Char('t'));
            type_query(&mut app, expected.slug());
            assert_eq!(app.theme_name, expected);
            let cache = Arc::clone(&app.highlighted[0][0]);
            render(&mut app, 60, 12);
            assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
            press(&mut app, KeyCode::Enter);
            assert!(app.theme_picker.is_none());
            assert_eq!(app.theme_name, expected);
            assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
            assert_eq!(
                state,
                (
                    app.visible.clone(),
                    app.selected,
                    app.occurrence,
                    app.scroll,
                    app.match_scroll_anchored,
                    app.horizontal,
                    app.focus,
                    app.query.clone(),
                    app.diff_enabled,
                    app.list,
                    app.status.clone(),
                )
            );
            assert!(!app.searching);
            assert_eq!(app.pair_diff.as_ref().unwrap().diff.left.as_ptr(), pair);
            assert!(Arc::ptr_eq(&app.highlighted[0][0], &app.highlighted[0][1]));
            assert_eq!(
                app.highlighted[0][0][0].spans[0].style.fg,
                Some(app.theme.keyword)
            );
            assert_eq!(
                app.highlighted[0][0][0].to_string(),
                original[0].to_string()
            );
            let cache = Arc::clone(&app.highlighted[0][0]);
            let (text, buffer) = render(&mut app, 160, 48);
            assert!(text.contains(app.theme_name.display_name()));
            assert_readable(&buffer, expected);
            assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
        }
        assert!(!Arc::ptr_eq(&original, &app.highlighted[0][0]));
        let applied = app.theme_name;
        press(&mut app, KeyCode::Char('?'));
        let cache = Arc::clone(&app.highlighted[0][0]);
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('T'));
        assert!(app.help);
        assert!(
            render(&mut app, 160, 48)
                .0
                .contains("Choose theme; type to filter")
        );
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('T'));
        assert!(app.searching);
        assert_eq!(app.query, "parserstT");
        assert_eq!(app.theme_name, applied);
        assert!(app.theme_picker.is_none());
        assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
    }

    fn type_query(app: &mut App, query: &str) {
        for c in query.chars() {
            assert_eq!(press(app, KeyCode::Char(c)), Action::Draw);
        }
    }

    #[test]
    fn picker_filters_display_names_and_slugs_case_insensitively() {
        let mut app = App::new(report(), ThemeName::default());
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.theme_picker.as_ref().unwrap().visible, ThemeName::all());
        for query in ["", "LIGHT", "dark", "a", "zz-no-such-theme", "日本語"]
            .into_iter()
            .map(str::to_owned)
            .chain(ThemeName::all().iter().flat_map(|name| {
                [
                    name.display_name().to_uppercase(),
                    name.slug().to_uppercase(),
                ]
            }))
        {
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('t'));
            type_query(&mut app, &query);
            let expected: Vec<_> = ThemeName::all()
                .iter()
                .copied()
                .filter(|name| {
                    name.display_name()
                        .to_lowercase()
                        .contains(&query.to_lowercase())
                        || name.slug().to_lowercase().contains(&query.to_lowercase())
                })
                .collect();
            let picker = app.theme_picker.as_ref().unwrap();
            assert_eq!(picker.visible, expected, "{query}");
            assert_eq!(picker.list.selected().is_some(), !expected.is_empty());
            if expected.is_empty() {
                assert!(render(&mut app, 80, 20).0.contains("No matching themes"));
                assert_eq!(press(&mut app, KeyCode::Enter), Action::Draw);
                for code in [KeyCode::Up, KeyCode::Down, KeyCode::Home, KeyCode::End] {
                    press(&mut app, code);
                }
                assert!(app.theme_picker.as_ref().unwrap().list.selected().is_none());
            }
            for _ in query.chars() {
                press(&mut app, KeyCode::Backspace);
            }
            press(&mut app, KeyCode::Backspace);
            let picker = app.theme_picker.as_ref().unwrap();
            assert!(picker.query.is_empty());
            assert_eq!(picker.visible, ThemeName::all());
            assert_eq!(picker.list.selected(), Some(0));
        }
    }

    #[test]
    fn picker_navigation_scrolls_and_renders_in_tiny_windows() {
        let mut app = App::new(report(), ThemeName::default());
        press(&mut app, KeyCode::Char('t'));
        let picker = app.theme_picker.as_ref().unwrap();
        assert_eq!(
            picker.visible[picker.list.selected().unwrap()],
            app.theme_name
        );
        assert!(
            render(&mut app, 80, 20)
                .0
                .contains(&format!("* {}", app.theme_name.display_name()))
        );
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.theme_picker.as_ref().unwrap().list.selected(), Some(0));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.theme_picker.as_ref().unwrap().list.selected(), Some(1));
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.theme_picker.as_ref().unwrap().list.selected(), Some(2));
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.theme_picker.as_ref().unwrap().list.selected(), Some(1));
        assert!(app.theme_picker.as_ref().unwrap().query.is_empty());
        type_query(&mut app, "a");
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Down);
        let picker = app.theme_picker.as_ref().unwrap();
        assert_eq!(picker.list.selected(), Some(picker.visible.len() - 1));
        let last = *picker.visible.last().unwrap();
        assert!(render(&mut app, 72, 8).0.contains(last.display_name()));
        assert!(app.theme_picker.as_ref().unwrap().list.offset() > 0);
        press(&mut app, KeyCode::Home);
        render(&mut app, 72, 8);
        assert_eq!(app.theme_picker.as_ref().unwrap().list.offset(), 0);
        for (width, height) in [(0, 0), (1, 1), (3, 2), (4, 4), (20, 6), (59, 20), (60, 12)] {
            assert_readable(&render(&mut app, width, height).1, app.theme_name);
        }
        press(&mut app, KeyCode::Esc);
        for &name in ThemeName::all() {
            app.set_theme(name);
            press(&mut app, KeyCode::Char('t'));
            assert_readable(&render(&mut app, 72, 20).1, name);
            press(&mut app, KeyCode::Esc);
        }
    }

    #[test]
    fn picker_previews_input_changes_and_restores_original_cache() {
        let mut app = App::new(context_report(), ThemeName::Dracula);
        let original_buffer = render(&mut app, 160, 48).1;
        let original = Arc::clone(&app.highlighted[0][0]);
        press(&mut app, KeyCode::Char('t'));
        assert!(Arc::ptr_eq(&original, &app.highlighted[0][0]));
        press(&mut app, KeyCode::Home);
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::End, KeyCode::Home] {
            press(&mut app, code);
            let picker = app.theme_picker.as_ref().unwrap();
            assert_eq!(
                app.theme_name,
                picker.visible[picker.list.selected().unwrap()]
            );
        }
        let target = ThemeName::all()
            .iter()
            .copied()
            .find(|&name| name != ThemeName::Dracula)
            .unwrap();
        type_query(&mut app, target.slug());
        let preview_name = app.theme_name;
        assert_eq!(preview_name, target);
        assert_ne!(preview_name, ThemeName::Dracula);
        let preview = Arc::clone(&app.highlighted[0][0]);
        assert!(!Arc::ptr_eq(&original, &preview));
        assert!(Arc::ptr_eq(&preview, &app.highlighted[0][1]));
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Tab] {
            press(&mut app, code);
            assert!(Arc::ptr_eq(&preview, &app.highlighted[0][0]));
        }
        type_query(&mut app, "!no-match");
        assert!(app.theme_picker.as_ref().unwrap().visible.is_empty());
        for code in [KeyCode::Enter, KeyCode::Down, KeyCode::Home, KeyCode::End] {
            press(&mut app, code);
            assert_eq!(app.theme_name, preview_name);
            assert!(Arc::ptr_eq(&preview, &app.highlighted[0][0]));
        }
        for size in [(160, 48), (60, 12), (1, 1)] {
            render(&mut app, size.0, size.1);
            assert!(Arc::ptr_eq(&preview, &app.highlighted[0][0]));
        }
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.theme_name, ThemeName::Dracula);
        assert!(Arc::ptr_eq(&original, &app.highlighted[0][0]));
        assert!(Arc::ptr_eq(&original, &app.highlighted[0][1]));
        assert_eq!(render(&mut app, 160, 48).1, original_buffer);

        press(&mut app, KeyCode::Char('t'));
        type_query(&mut app, target.slug());
        let preview = Arc::clone(&app.highlighted[0][0]);
        press(&mut app, KeyCode::Enter);
        assert!(app.theme_picker.is_none());
        assert_eq!(app.theme_name, preview_name);
        assert!(Arc::ptr_eq(&preview, &app.highlighted[0][0]));
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.theme_picker.as_ref().unwrap().original, preview_name);
        press(&mut app, KeyCode::Esc);
        assert!(Arc::ptr_eq(&preview, &app.highlighted[0][0]));
    }

    #[test]
    fn picker_overlays_dimmed_live_syntax_with_full_brightness_modal() {
        assert_eq!(
            dim_foreground(Color::Rgb(200, 100, 40), Color::Rgb(40, 20, 0)),
            Color::Rgb(160, 80, 30)
        );
        assert_eq!(dim_foreground(Color::Reset, Color::Reset), Color::Reset);
        for &name in ThemeName::all() {
            let mut app = App::new(report(), ThemeName::Dracula);
            press(&mut app, KeyCode::Char('t'));
            type_query(&mut app, name.slug());
            assert_eq!(app.theme_name, name);
            let cache = Arc::clone(&app.highlighted[0][0]);
            let mut picker = app.theme_picker.take().unwrap();
            let (_, browser) = render(&mut app, 160, 48);
            // Independently render the modal onto a clean buffer to catch leaked
            // background text, dimming, or modifiers inside the popup.
            let mut terminal = Terminal::new(TestBackend::new(160, 48)).unwrap();
            let modal = terminal
                .draw(|frame| {
                    draw_theme_picker(frame, &mut picker, &app.theme, frame.area());
                })
                .unwrap()
                .buffer
                .clone();
            app.theme_picker = Some(picker);
            let (_, overlay) = render(&mut app, 160, 48);
            let popup = Rect::new(44, 14, 72, 20);
            let mut syntax_seen = false;
            for y in 0..48 {
                for x in 0..160 {
                    let cell = &overlay[(x, y)];
                    if popup.contains((x, y).into()) {
                        assert_eq!(cell, &modal[(x, y)], "{name:?} at {x},{y}");
                    } else {
                        let mut expected = browser[(x, y)].clone();
                        if expected.symbol() == "f" && expected.fg == app.theme.keyword {
                            syntax_seen = true;
                            assert_ne!(cell.fg, expected.fg);
                        }
                        expected.fg = dim_foreground(expected.fg, expected.bg);
                        assert_eq!(cell, &expected, "{name:?} at {x},{y}");
                    }
                }
            }
            assert!(syntax_seen, "{name:?}: no background syntax exercised");
            assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
        }
        let mut app = App::new(report(), ThemeName::Dracula);
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Down);
        let (text, _) = render(&mut app, 160, 48);
        assert!(text.contains("* Dracula"));
        assert_eq!(text.matches("* Dracula").count(), 1);
        assert_ne!(app.theme_name, ThemeName::Dracula);
        assert!(!text.contains(&format!("* {}", app.theme_name.display_name())));
    }

    #[test]
    fn picker_isolates_keys_and_cancel_preserves_browser_and_caches() {
        let mut app = App::new(context_report(), ThemeName::default());
        app.query = "parsers".into();
        app.filter();
        render(&mut app, 160, 48);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('L'));
        app.status = Some((true, "keep status".into()));
        let state = (
            app.selected,
            app.occurrence,
            app.scroll,
            app.match_scroll_anchored,
            app.horizontal,
            app.page,
            app.focus,
            app.list,
            app.status.clone(),
        );
        let cache = Arc::clone(&app.highlighted[0][0]);
        let pair = app.pair_diff.as_ref().unwrap().diff.left.as_ptr();
        press(&mut app, KeyCode::Char('T'));
        assert!(app.theme_picker.is_none());
        press(&mut app, KeyCode::Char('t'));
        type_query(&mut app, "qjktT?d/onpHL[]gG");
        assert_eq!(
            app.theme_picker.as_ref().unwrap().query,
            "qjktT?d/onpHL[]gG"
        );
        for code in [
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Enter,
        ] {
            assert_eq!(press(&mut app, code), Action::Draw);
        }
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        render(&mut app, 60, 12);
        press(&mut app, KeyCode::Esc);
        assert!(app.theme_picker.is_none());
        assert_eq!(app.theme_name, ThemeName::default());
        assert_eq!(
            state,
            (
                app.selected,
                app.occurrence,
                app.scroll,
                app.match_scroll_anchored,
                app.horizontal,
                app.page,
                app.focus,
                app.list,
                app.status.clone()
            )
        );
        assert_eq!(app.query, "parsers");
        assert_eq!(app.visible, vec![0]);
        assert!(app.diff_enabled);
        assert!(!app.help && !app.searching);
        assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
        assert_eq!(pair, app.pair_diff.as_ref().unwrap().diff.left.as_ptr());
        press(&mut app, KeyCode::Char('t'));
        assert!(app.theme_picker.as_ref().unwrap().query.is_empty());
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.theme_name, ThemeName::default());
        assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
    }

    fn report() -> BrowserReport {
        BrowserReport {
            mode: "semantic".into(),
            files: 3,
            groups: vec![
                MatchGroup {
                    title: "Duplicate parsers".into(),
                    description: "Original Rust source".into(),
                    occurrences: vec![
                        snippet("parse", "src/one.rs"),
                        snippet("parse_again", "src/two.rs"),
                    ],
                    edges: vec![PairEvidence {
                        left: 0,
                        right: 1,
                        cosine: 0.9876,
                        exact_body: true,
                    }],
                },
                MatchGroup {
                    title: "Unicode λ".into(),
                    description: String::new(),
                    occurrences: vec![snippet("日本語", "src/日本語.rs")],
                    edges: vec![],
                },
            ],
        }
    }

    fn snippet(name: &str, path: &str) -> Snippet {
        let source = "fn parse() {\n    /* first\n       second */\n    let 日本語 = r#\"first\nsecond\"#;\n    consume(42);\n}\n";
        Snippet {
            name: name.into(),
            path: PathBuf::from(path),
            start_line: 42,
            start_column: 5,
            source: source.into(),
            display_source: Arc::from(source),
            display_start_line: 42,
            match_start_line: 42,
            match_end_line: 42 + source.lines().count() - 1,
            test_context: false,
            warning: None,
        }
    }

    fn set_source(snippet: &mut Snippet, source: &str) {
        snippet.source = source.into();
        snippet.display_source = Arc::from(source);
        snippet.display_start_line = snippet.start_line;
        snippet.match_start_line = snippet.start_line;
        snippet.match_end_line = snippet.start_line + source.lines().count().saturating_sub(1);
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn render(app: &mut App, width: u16, height: u16) -> (String, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        // Inspect the complete frame: terminal diffing omits continuation cells
        // hidden by wide glyphs, so TestBackend does not receive their styles.
        let buffer = terminal
            .draw(|frame| draw(frame, app))
            .unwrap()
            .buffer
            .clone();
        let text = buffer
            .content
            .chunks(usize::from(width).max(1))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (text, buffer)
    }

    fn context_report() -> BrowserReport {
        let mut data = report();
        let full = (1..=80)
            .map(|n| format!("fn line_{n}() {{ let 日本語 = {n}; }}\n"))
            .collect::<String>();
        for (index, snippet) in data.groups[0].occurrences.iter_mut().enumerate() {
            let start = 10 + index * 20;
            snippet.start_line = start;
            let body = full
                .lines()
                .skip(start - 1)
                .take(2)
                .collect::<Vec<_>>()
                .join("\n");
            set_source(snippet, &body);
            // Equal content in distinct allocations must share highlighting too.
            snippet.display_source = Arc::from(full.as_str());
            snippet.display_start_line = 1;
        }
        data
    }

    #[test]
    fn default_cards_show_dim_context_and_diff_only_on_matched_lines() {
        let mut app = App::new(context_report(), ThemeName::Dracula);
        let (text, buffer) = render(&mut app, 160, 48);
        for expected in [
            "  7 │ fn line_7",
            " 10 − fn line_10",
            " 11 − fn line_11",
            " 12 │ fn line_12",
            " 27 │ fn line_27",
            " 30 + fn line_30",
            " 31 + fn line_31",
            " 32 │ fn line_32",
            "context dimmed · matched lines 10–11",
        ] {
            assert!(text.contains(expected), "missing {expected:?}\n{text}");
        }
        for n in [7, 12, 27, 32] {
            let needle = format!("fn line_{n}()");
            let row = buffer
                .content
                .chunks(160)
                .find(|row| {
                    row.iter()
                        .map(|c| c.symbol())
                        .collect::<String>()
                        .contains(&needle)
                })
                .unwrap();
            let code = row
                .windows(2)
                .position(|cells| cells[0].symbol() == "f" && cells[1].symbol() == "n")
                .unwrap();
            let end = row.iter().rposition(|c| c.symbol() == "}").unwrap();
            assert!(
                row[code..=end]
                    .iter()
                    .all(|c| c.fg == dracula().muted.fg.unwrap())
            );
            assert!(
                row[code..=end]
                    .iter()
                    .all(|c| c.bg == dracula().panel.bg.unwrap()),
                "context line {n}: {:?}",
                &row[code..=end]
            );
        }
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.fg == dracula().keyword && c.bg == dracula().changed.row.bg.unwrap())
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.fg == dracula().number && c.bg == dracula().changed.row.bg.unwrap())
        );
        // Dimming visible context must not mutate the shared syntax cache.
        assert_eq!(
            app.highlighted[0][0][6].spans[0].style.fg,
            Some(dracula().keyword)
        );
        assert_eq!(app.scroll, 6);
    }

    #[test]
    fn source_scrolls_beyond_match_and_navigation_refocuses_each_occurrence() {
        let mut app = App::new(context_report(), ThemeName::Dracula);
        render(&mut app, 120, 24);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.scroll, 7);
        press(&mut app, KeyCode::Char('G'));
        let (text, buffer) = render(&mut app, 120, 24);
        assert!(text.contains(" 80 │ fn line_80"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !dracula_diff_background(c.bg))
        );
        assert_eq!(press(&mut app, KeyCode::Enter), Action::Open);
        assert_eq!(
            (
                app.snippet().unwrap().start_line,
                app.snippet().unwrap().start_column
            ),
            (10, 5)
        );
        press(&mut app, KeyCode::Char('n'));
        render(&mut app, 120, 24);
        assert_eq!(app.scroll, 26);
        press(&mut app, KeyCode::Char('p'));
        render(&mut app, 120, 24);
        assert_eq!(app.scroll, 6);
        press(&mut app, KeyCode::Char('g'));
        assert!(render(&mut app, 120, 24).0.contains("  1 │ fn line_1"));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        render(&mut app, 120, 24);
        assert_eq!(app.scroll, 6);
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.scroll, 6);
        app.query = "Duplicate".into();
        app.filter();
        assert_eq!(app.scroll, 6);
        // With fewer than four source rows, still show the match on selection.
        app.cycle(true);
        let (text, _) = render(&mut app, 100, 14);
        assert!(
            text.contains(" 30 − fn line_30"),
            "scroll={} page={}\n{text}",
            app.scroll,
            app.page
        );
    }

    #[test]
    fn selected_match_stays_visible_on_resize_including_one_line_viewports() {
        let mut app = App::new(context_report(), ThemeName::Dracula);
        for occurrence in 0..2 {
            let matched = 10 + occurrence * 20;
            for height in [48, 15, 14, 13, 24, 48] {
                let (text, _) = render(&mut app, 120, height);
                assert!(
                    text.contains(&format!("{matched:>3} − fn line_{matched}")),
                    "height={height}, scroll={}, page={}\n{text}",
                    app.scroll,
                    app.page
                );
                assert_eq!(app.scroll, matched - 1 - 3.min(app.page - 1));
                assert!(app.match_scroll_anchored);
                if height == 48 {
                    let other = if occurrence == 0 { 30 } else { 10 };
                    assert!(text.contains(&format!("{:>3} │ fn line_{}", other - 3, other - 3)));
                    assert!(text.contains(&format!("{other:>3} + fn line_{other}")));
                }
            }
            app.cycle(true);
        }
    }

    #[test]
    fn resize_preserves_intentional_source_scrolling_until_selection_changes() {
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            let mut app = App::new(context_report(), ThemeName::Dracula);
            render(&mut app, 120, 24);
            app.focus = Focus::Code;
            press(&mut app, key);
            assert!(!app.match_scroll_anchored);
            let scroll = app.scroll;
            render(&mut app, 120, 13);
            assert_eq!(app.scroll, scroll, "{key:?}");
            render(&mut app, 120, 48);
            assert_eq!(app.scroll, scroll.min(app.max_scroll()), "{key:?}");
            press(&mut app, KeyCode::Char('n'));
            let (text, _) = render(&mut app, 120, 13);
            assert_eq!(app.scroll, 29);
            assert!(app.match_scroll_anchored);
            assert!(text.contains(" 30 − fn line_30"));
        }
    }

    #[test]
    fn context_highlights_are_shared_and_rendering_never_reloads_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context.rs");
        let mut data = context_report();
        std::fs::write(
            &path,
            data.groups[0].occurrences[0].display_source.as_bytes(),
        )
        .unwrap();
        for snippet in &mut data.groups[0].occurrences {
            snippet.path = path.clone();
        }
        let mut app = App::new(data, ThemeName::Dracula);
        assert!(Arc::ptr_eq(&app.highlighted[0][0], &app.highlighted[0][1]));
        let cache = Arc::clone(&app.highlighted[0][0]);
        std::fs::write(&path, "changed on disk").unwrap();
        assert!(render(&mut app, 120, 24).0.contains("fn line_10"));
        std::fs::remove_file(&path).unwrap();
        app.cycle(true);
        assert!(render(&mut app, 160, 48).0.contains("fn line_30"));
        assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
        press(&mut app, KeyCode::Char('t'));
        type_query(&mut app, "light");
        press(&mut app, KeyCode::Enter);
        assert!(!Arc::ptr_eq(&cache, &app.highlighted[0][0]));
        assert!(Arc::ptr_eq(&app.highlighted[0][0], &app.highlighted[0][1]));
        assert!(render(&mut app, 160, 48).0.contains("fn line_30"));
        assert!(
            app.report.groups[0].occurrences[0]
                .source
                .starts_with("fn line_10")
        );
    }

    #[test]
    fn missing_or_stale_context_fallback_uses_snapshot_and_real_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        for warning in ["source missing", "stale source: file changed"] {
            if warning.starts_with("stale") {
                std::fs::write(&path, "unrelated disk content").unwrap();
            }
            let mut data = report();
            data.groups[0].occurrences.truncate(1);
            let snippet = &mut data.groups[0].occurrences[0];
            snippet.path = path.clone();
            snippet.warning = Some(warning.into());
            let mut app = App::new(data, ThemeName::Dracula);
            let (text, buffer) = render(&mut app, 120, 24);
            assert!(text.contains(&format!("WARNING: {warning}")));
            assert!(text.contains(" 42 │ fn parse()"));
            assert!(!text.contains("unrelated disk content"));
            assert!(buffer.content.iter().any(|c| c.fg == dracula().keyword));
            assert_eq!(app.scroll, 0);
        }
    }

    #[test]
    fn horizontal_scroll_preserves_gutter_and_unicode_columns() {
        let mut data = report();
        set_source(
            &mut data.groups[0].occurrences[0],
            "日e\u{301}本tail\n// next",
        );
        let mut app = App::new(data, ThemeName::Dracula);
        app.focus = Focus::Code;
        press(&mut app, KeyCode::Char('d'));
        for (offset, expected) in [(0, "日"), (2, "e\u{301}"), (3, "本"), (5, "t")] {
            app.horizontal = offset;
            let (text, buffer) = render(&mut app, 100, 24);
            assert!(text.contains(" 42 │ "));
            assert!(text.contains(" 43 │ "));
            let row = buffer
                .content
                .chunks(100)
                .find(|row| {
                    row.iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains(" 42 │ ")
                })
                .unwrap();
            let gutter = row
                .windows(6)
                .position(|cells| {
                    cells.iter().map(|cell| cell.symbol()).collect::<String>() == " 42 │ "
                })
                .unwrap();
            assert_eq!(row[gutter + 6].symbol(), expected, "offset {offset}");
            assert_eq!(row[gutter + 1].fg, dracula().muted.fg.unwrap());
            assert_eq!(row[gutter + 6].fg, dracula().foreground);
        }
        app.horizontal = 0;
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(app.horizontal, 4);
        assert_eq!(app.focus, Focus::Code);
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(app.horizontal, 0);
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(app.horizontal, 4);
        press(&mut app, KeyCode::Char('H'));
        assert_eq!(app.horizontal, 0);
        app.horizontal = u16::MAX;
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(app.horizontal, u16::MAX);
        render(&mut app, 60, 12);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.horizontal, 0);
        for key in [
            KeyCode::Char('j'),
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            app.focus = Focus::Groups;
            app.selected = 0;
            app.horizontal = 8;
            press(&mut app, key);
            assert_eq!(app.horizontal, 0, "{key:?}");
        }
        app.horizontal = 8;
        app.filter();
        assert_eq!(app.horizontal, 0);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.focus, Focus::Code);
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.focus, Focus::Groups);
    }

    fn diff_app(a: &str, b: &str) -> App {
        let mut data = report();
        set_source(&mut data.groups[0].occurrences[0], a);
        set_source(&mut data.groups[0].occurrences[1], b);
        App::new(data, ThemeName::Dracula)
    }

    fn source_rows(buffer: &ratatui::buffer::Buffer, needle: &str) -> Vec<(String, Color)> {
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .filter_map(|row| {
                let text = row.iter().map(|c| c.symbol()).collect::<String>();
                if !text.contains(needle) {
                    return None;
                }
                let color = row
                    .iter()
                    .find(|c| dracula_diff_background(c.bg))
                    .map_or(dracula().panel.bg.unwrap(), |c| c.bg);
                Some((text, color))
            })
            .collect()
    }

    #[test]
    fn diff_insertion_preserves_later_lines_and_toggle_and_source() {
        let a = "fn start() {}\nfn keep() {}\nfn end() {}\n";
        let b = "fn start() {}\nfn inserted() {}\nfn keep() {}\nfn end() {}\n";
        let mut app = diff_app(a, b);
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("compared #1 ↔ #2; − only #1 / + only #2"));
        assert!(text.contains("43 + fn inserted()"));
        assert_eq!(source_rows(&buffer, "fn keep()").len(), 2);
        assert!(
            source_rows(&buffer, "fn keep()")
                .iter()
                .all(|(_, bg)| *bg == dracula().panel.bg.unwrap())
        );
        assert_eq!(
            source_rows(&buffer, "fn inserted()")[0].1,
            dracula().added.row.bg.unwrap()
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.bg == dracula().added.row.bg.unwrap() && c.fg == dracula().keyword)
        );
        press(&mut app, KeyCode::Char('d'));
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("d OFF"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !dracula_diff_background(c.bg))
        );
        press(&mut app, KeyCode::Char('d'));
        assert!(
            render(&mut app, 120, 40)
                .1
                .content
                .iter()
                .any(|c| c.bg == dracula().added.row.bg.unwrap())
        );
        assert_eq!(app.report.groups[0].occurrences[0].source, a);
        assert_eq!(app.report.groups[0].occurrences[1].source, b);
    }

    #[test]
    fn unicode_changed_lines_keep_syntax_and_pan_markers() {
        let mut app = diff_app("let 日本語 = \"e\u{301}\";\n", "let 日本語 = \"é\";\n");
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("42 − let 日"));
        assert!(text.contains("42 + let 日"));
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.bg == dracula().changed.row.bg.unwrap() && c.fg == dracula().string)
        );
        app.focus = Focus::Code;
        app.horizontal = 4;
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("42 − 日"));
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "日" && c.bg == dracula().changed.row.bg.unwrap())
        );
        let mut equal = diff_app("let 日本語 = 42;\n", "let 日本語 = 42;\n");
        assert!(
            render(&mut equal, 120, 40)
                .1
                .content
                .iter()
                .all(|c| !dracula_diff_background(c.bg))
        );
    }

    #[test]
    fn actual_pair_wraps_and_remains_explicit_in_single_card() {
        let mut app = diff_app("a\n", "b\n");
        let mut third = snippet("third", "src/third.rs");
        set_source(&mut third, "b\n");
        let mut data = app.report;
        data.groups[0].occurrences.push(third);
        app = App::new(data, ThemeName::Dracula);
        app.cycle(true);
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("compared #2 ↔ #3"));
        assert!(text.contains("No pair similarity: #2 ↔ #3 · no direct edge"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !dracula_diff_background(c.bg))
        );
        app.cycle(true);
        let (text, _) = render(&mut app, 120, 40);
        assert!(text.contains("compared #3 ↔ #1"));
        assert!(text.contains("42 − b"));
        assert!(text.contains("42 + a"));
        let (text, _) = render(&mut app, 60, 12);
        assert!(text.contains("compared #3 ↔ #1"));
        assert!(text.contains("− only #3 / + only #1"));
        app.cycle(true);
        assert_eq!(
            app.pair_diff.as_ref().map(|p| (p.left, p.right)),
            Some((0, 1))
        );
        app.move_group(true);
        assert!(app.pair_diff.is_none());
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("No comparison pair"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !dracula_diff_background(c.bg))
        );
    }

    #[test]
    fn group_navigation_and_filter_refresh_the_pair_cache() {
        let mut data = report();
        data.groups.push(MatchGroup {
            title: "Third group".into(),
            description: String::new(),
            occurrences: vec![
                snippet("left", "src/left.rs"),
                snippet("right", "src/right.rs"),
            ],
            edges: vec![],
        });
        set_source(
            &mut data.groups[2].occurrences[0],
            "fn removed() {}\nfn stay() {}\n",
        );
        set_source(&mut data.groups[2].occurrences[1], "fn stay() {}\n");
        let mut app = App::new(data, ThemeName::Dracula);
        press(&mut app, KeyCode::End);
        assert!(app.pair_diff.is_none());
        press(&mut app, KeyCode::Home);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 2);
        let (_, buffer) = render(&mut app, 120, 40);
        assert_eq!(
            source_rows(&buffer, "fn removed()")[0].1,
            dracula().removed.row.bg.unwrap()
        );
        assert!(
            source_rows(&buffer, "fn stay()")
                .iter()
                .all(|(_, bg)| *bg == dracula().panel.bg.unwrap())
        );
        app.page = 1;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 0);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 2);
        app.query = "Third group".into();
        app.filter();
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 2);
        app.query = "missing group".into();
        app.filter();
        assert!(app.pair_diff.is_none());
    }

    #[test]
    fn bounded_fallback_is_visible_and_cache_survives_draw() {
        let mut app = diff_app(&"a\n".repeat(1100), &"b\n".repeat(1100));
        let ptr = app.pair_diff.as_ref().unwrap().diff.left.as_ptr();
        let (text, buffer) = render(&mut app, 60, 12);
        assert!(text.contains("LIMIT: compared #1 ↔ #2"));
        assert_eq!(app.pair_diff.as_ref().unwrap().diff.left.as_ptr(), ptr);
        assert_eq!(buffer.area.height, 12);
        let (_, buffer) = render(&mut app, 120, 40);
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.bg == dracula().changed.row.bg.unwrap())
        );
        press(&mut app, KeyCode::Char('?'));
        let text = render(&mut app, 100, 30).0;
        assert!(text.contains("Toggle exact-text diff"));
        assert!(text.contains("LIMIT: conservative marks"));
    }

    #[test]
    fn display_paths_are_relative_or_tail_shortened_without_changing_editor_path() {
        let inside = snippet("parse", "/work/project/src/file.rs");
        let cwd = Some(Path::new("/work/project"));
        assert_eq!(display_location(&inside, cwd, 80), "src/file.rs:42:5");
        let outside = snippet("parse", "/elsewhere/very/long/parent/file.rs");
        let shortened = display_location(&outside, cwd, 22);
        assert!(shortened.starts_with('…'));
        assert!(shortened.ends_with("file.rs:42:5"));
        assert_eq!(Line::from(shortened).width(), 22);
        assert_eq!(
            outside.path,
            Path::new("/elsewhere/very/long/parent/file.rs")
        );
        let sibling = snippet("parse", "/work/project-other/src/file.rs");
        assert!(display_location(&sibling, cwd, 80).starts_with("/work/project-other/"));
        let unicode = snippet("parse", "/elsewhere/日本語/e\u{301}.rs");
        for width in 0..60 {
            let location = display_location(&unicode, None, width);
            assert!(Line::from(location.as_str()).width() <= usize::from(width));
            assert!(!location.starts_with("…\u{301}"));
        }
        assert!(display_location(&unicode, None, 18).ends_with("e\u{301}.rs:42:5"));
    }

    #[test]
    fn syntax_evidence_context_and_warning_labels() {
        let mut data = report();
        data.mode = "syntax".into();
        data.groups[0].edges.clear();
        data.groups[0].occurrences[1].test_context = true;
        let mut app = App::new(data, ThemeName::Dracula);
        assert_eq!(
            evidence(&app, 0, 0),
            "No pair similarity: #1 ↔ #2 · no direct edge"
        );
        let (text, _) = render(&mut app, 160, 40);
        assert!(text.contains("No pair similarity: #1 ↔ #2 · no direct edge"));
        assert!(text.contains("context dimmed · matched lines"));
        assert!(text.contains("test context"));
        assert!(!text.contains("production"));
        assert!(!text.contains("No pair evidence"));
        app.report.groups[0].occurrences[0].warning = Some("stale source: file changed".into());
        let (text, _) = render(&mut app, 100, 24);
        assert!(text.contains("WARNING: stale source: file changed"));
        assert!(!text.contains("source · WARNING"));
        app.report.mode = "semantic".into();
        assert_eq!(
            evidence(&app, 0, 0),
            "No pair similarity: #1 ↔ #2 · no direct edge"
        );
    }

    fn sorting_report() -> BrowserReport {
        let mut data = report();
        data.groups = [("z", 2, 20), ("b", 3, 2), ("a", 3, 2), ("c", 1, 4)]
            .into_iter()
            .map(|(name, count, length)| MatchGroup {
                title: name.into(),
                description: String::new(),
                occurrences: (0..count)
                    .map(|i| {
                        let mut s = snippet(
                            name,
                            &format!("/nonexistent/{name}/{}.rs", if name == "b" { 0 } else { i }),
                        );
                        s.match_end_line = s.match_start_line + length - 1;
                        s
                    })
                    .collect(),
                edges: vec![],
            })
            .collect();
        data
    }

    #[test]
    fn sorts_cached_metadata_with_path_ties_and_keeps_report_pair_and_scroll() {
        let mut app = App::new(sorting_report(), ThemeName::Dracula);
        assert_eq!(app.visible, [2, 1, 0, 3]);
        assert_eq!(app.metadata[0].lines, 40);
        assert_eq!(app.metadata[1].files, 1);
        app.cycle(true);
        app.scroll = 7;
        app.horizontal = 9;

        app.match_scroll_anchored = false;
        let pair = app.pair_diff.as_ref().unwrap().diff.left.as_ptr();
        let cache = Arc::clone(&app.highlighted[2][1]);
        for (sort, expected) in [
            (GroupSort::Lines, vec![0, 2, 1, 3]),
            (GroupSort::Files, vec![2, 0, 1, 3]),
            (GroupSort::Path, vec![2, 1, 3, 0]),
            (GroupSort::Occurrences, vec![2, 1, 0, 3]),
        ] {
            app.apply_sort(sort);
            assert_eq!(app.visible, expected);
            assert_eq!(app.group_index(), Some(2));
            assert_eq!((app.occurrence, app.scroll, app.horizontal), (1, 7, 9));
            assert!(!app.match_scroll_anchored);
            assert_eq!(app.pair_diff.as_ref().unwrap().diff.left.as_ptr(), pair);
            assert!(Arc::ptr_eq(&cache, &app.highlighted[2][1]));
            assert_eq!(app.report.groups[0].title, "z");
        }
        app.apply_sort(GroupSort::Lines);
        app.query = "/nonexistent".into();
        app.filter();
        assert_eq!(app.visible, [0, 2, 1, 3]);
        app.query = "missing".into();
        app.filter();
        app.apply_sort(GroupSort::Files);
        assert!(app.visible.is_empty());
        assert!(app.group_index().is_none());
        app.query.clear();
        app.filter();
        assert_eq!(app.visible, [2, 0, 1, 3]);
        // All paths are nonexistent: rendering and sorting use only cached data.
        render(&mut app, 100, 24);
        let mut data = sorting_report();
        data.groups.clear();
        let mut empty = App::new(data, ThemeName::Dracula);
        empty.apply_sort(GroupSort::Path);
        assert!(empty.visible.is_empty());
    }

    #[test]
    fn sort_modal_is_isolated_and_theme_preview_still_cancels() {
        let mut app = App::new(sorting_report(), ThemeName::Dracula);
        let visible = app.visible.clone();
        press(&mut app, KeyCode::Char('s'));
        for key in ['q', 't', '/', 'n', '.', '?'] {
            assert_eq!(
                app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
                Action::Draw
            );
        }
        assert!(app.theme_picker.is_none());
        assert!(!app.help && !app.searching);
        assert_eq!(app.occurrence, 0);
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.sort_picker, Some(1));
        assert_eq!(app.visible, visible);
        let saved = (app.scroll, app.page, app.list);
        for (w, h) in [(1, 1), (20, 6), (100, 24)] {
            render(&mut app, w, h);
            assert_eq!((app.scroll, app.page, app.list), saved);
        }
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.sort, GroupSort::Occurrences);
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.sort, GroupSort::Lines);
        let visible = app.visible.clone();
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('s'));
        assert!(app.sort_picker.is_none());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.theme_name, ThemeName::Dracula);
        assert_eq!(app.visible, visible);
    }

    fn card_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cards_group_shared_context_and_file_headers() {
        let mut data = report();
        data.groups[0].occurrences = vec![
            snippet("first", "/work/project/src/parser.rs"),
            snippet("second", "/work/project/src/parser.rs"),
        ];
        let mut app = App::new(data, ThemeName::Dracula);
        app.cwd = Some(PathBuf::from("/work/project"));
        let lines = group_card(&app, 0, 60);
        let text = card_text(&lines);
        assert_eq!(text.matches("src").count(), 1);
        assert_eq!(text.matches("parser.rs").count(), 1);
        assert!(text.contains("└─ parser.rs"));
        assert!(text.contains("first:42"));
        assert!(text.contains("second:42"));
        assert_eq!(lines[1].style, app.theme.muted);
        assert!(lines.last().unwrap().to_string().is_empty());

        app.report.groups[0].occurrences[1].path = "/work/project/src/other/parser.rs".into();
        let text = card_text(&group_card(&app, 0, 60));
        assert!(text.contains("├─ parser.rs\n│  ▸ first:42"));
        assert!(text.contains("└─ other/parser.rs"));
        assert_eq!(text.matches("src").count(), 1);
        assert_eq!(
            shared_parent(&[Path::new("src/foo/a.rs"), Path::new("src/foobar/b.rs")]),
            Path::new("src")
        );
        assert_eq!(
            shared_parent(&[Path::new("a.rs"), Path::new("elsewhere/b.rs")]),
            Path::new("")
        );
        assert_eq!(
            shared_parent(&[Path::new("/a.rs"), Path::new("b.rs")]),
            Path::new("")
        );
    }

    #[test]
    fn selection_and_occurrence_navigation_keep_cards_compact_and_stable() {
        let mut data = report();
        data.groups[0].occurrences = (0..5)
            .map(|i| {
                snippet(
                    &format!("function_{i}"),
                    if i < 2 {
                        "src/same.rs"
                    } else if i == 2 {
                        "src/other.rs"
                    } else {
                        "src/last.rs"
                    },
                )
            })
            .collect();
        let mut app = App::new(data, ThemeName::Dracula);
        let initial = group_card(&app, 0, 60);
        let height = initial.len();
        assert!(card_text(&initial).contains("+3 occurrences"));
        assert!(!card_text(&initial).contains("function_2"));
        let other = group_card(&app, 1, 60);
        for i in 0..5 {
            assert_eq!(app.occurrence, i);
            let card = group_card(&app, 0, 60);
            assert_eq!(card.len(), height);
            assert!(card_text(&card).contains(&format!("▸ function_{i}")));
            assert_eq!(group_card(&app, 1, 60), other);
            press(&mut app, KeyCode::Char('n'));
        }
        assert_eq!(app.occurrence, 0);
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.occurrence, 4);
        assert!(card_text(&group_card(&app, 0, 60)).contains("+4 occurrences"));
        press(&mut app, KeyCode::Down);
        let unselected = group_card(&app, 0, 60);
        assert_eq!(unselected.len(), height);
        assert!(card_text(&unselected).contains("function_0"));
        assert!(!card_text(&unselected).contains("function_4"));
        press(&mut app, KeyCode::Up);
        assert_eq!(group_card(&app, 0, 60), initial);
        for (w, h) in [(60, 12), (80, 20), (160, 48)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn long_unicode_paths_and_names_elide_without_wrapping() {
        let mut data = report();
        data.groups[0].occurrences = (0..3)
            .map(|i| {
                snippet(
                    &format!("function_{i}_{}", "日本e\u{301}".repeat(20)),
                    &format!("/work/{}/日本e\u{301}.rs", "parent/".repeat(20)),
                )
            })
            .collect();
        let mut app = App::new(data, ThemeName::Dracula);
        let height = group_card(&app, 0, 80).len();
        for width in 0..80 {
            for _ in 0..3 {
                let card = group_card(&app, 0, width);
                assert_eq!(card.len(), height);
                assert!(card.iter().all(|line| line.width() <= usize::from(width)));
                assert!(!card_text(&card).contains("…\u{301}"));
                press(&mut app, KeyCode::Char('n'));
            }
        }
        let card = group_card(&app, 0, 24);
        assert!(card[1].to_string().starts_with("/work/"));
        assert!(card[1].to_string().contains('…'));
        assert!(card[1].to_string().ends_with("parent"));
        assert!(card_text(&card).contains("日本e\u{301}.rs"));
        assert!(card_text(&card).contains("function_0_"));
        let location = Line::styled("long/日本e\u{301}.rs", app.theme.muted);
        assert_eq!(compact_location(location, 5).to_string(), "…e\u{301}.rs");
    }

    #[test]
    fn selected_card_retains_semantic_colors_without_bold() {
        for &name in ThemeName::all() {
            let mut app = App::new(report(), name);
            let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
            let buffer = terminal
                .draw(|frame| {
                    frame.render_widget(Block::default().style(app.theme.base), frame.area());
                    draw_groups(frame, &mut app, frame.area());
                })
                .unwrap()
                .buffer
                .clone();
            assert_readable(&buffer, name);
            // Summary, shared directory, file, function, line number.
            for (x, y, fg) in [
                (2, 1, app.theme.accent.fg.unwrap()),
                (2, 2, app.theme.muted.fg.unwrap()),
                (5, 3, app.theme.foreground),
                (7, 4, app.theme.function),
                (12, 4, app.theme.muted.fg.unwrap()),
            ] {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.fg, fg, "{name:?}: {cell:?}");
                assert_eq!(Some(cell.bg), group_selection(&app.theme).bg);
                assert!(!cell.modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn card_summary_uses_singular_and_plural_counts() {
        for (count, expected) in [
            (0, "0 occurrences · 0 lines · 0 files"),
            (1, "1 occurrence · 1 line · 1 file"),
            (2, "2 occurrences · 2 lines · 2 files"),
        ] {
            let mut data = report();
            data.groups[0].occurrences = (0..count)
                .map(|i| {
                    let mut occurrence = snippet("parse", &format!("src/file_{i}.rs"));
                    occurrence.match_end_line = occurrence.match_start_line;
                    occurrence
                })
                .collect();
            let app = App::new(data, ThemeName::Dracula);
            assert_eq!(group_card(&app, 0, 80)[0].to_string(), expected);
        }
    }

    #[test]
    fn sidebar_width_does_not_overflow() {
        assert_eq!(sidebar_width(60), 24);
        assert_eq!(sidebar_width(100), 40);
        assert_eq!(sidebar_width(u16::MAX), 80);
    }

    #[test]
    fn browser_snapshot_headings_pair_source_and_colors() {
        let mut app = App::new(report(), ThemeName::Dracula);
        let (text, buffer) = render(&mut app, 120, 40);
        for expected in [
            "rsdup",
            "occurrences ↓ · 2/2",
            "Match #1",
            "#1 parse",
            "#2 parse_again",
            "src/one.rs:42:5",
            "42 │ fn parse()",
            "same-token",
            "cosine 0.988",
            "s sort",
        ] {
            assert!(text.contains(expected), "missing {expected:?}\n{text}");
        }
        for color in [
            dracula().keyword,
            dracula().string,
            dracula().number,
            dracula().function,
        ] {
            assert!(
                buffer.content.iter().any(|cell| cell.fg == color),
                "missing {color:?}"
            );
        }
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| Some(cell.bg) == group_selection(&dracula()).bg)
        );
        assert!(!text.contains("equivalence"));
    }

    #[test]
    fn multiline_tokens_keep_styles_and_original_text() {
        let source = "/* a\nb */\nlet s = r#\"a\nb\"#;\n";
        let lines = highlight(source, &Theme::new(ThemeName::Dracula));
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[1].spans[0].style.fg,
            Some(dracula().muted.fg.unwrap())
        );
        assert_eq!(lines[3].spans[0].style.fg, Some(dracula().string));
        let recovered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(recovered, source.trim_end_matches('\n'));
    }

    #[test]
    fn incremental_unicode_search_cancel_and_selection() {
        let mut app = App::new(report(), ThemeName::Dracula);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.group_index(), Some(1));
        press(&mut app, KeyCode::Char('/'));
        for c in "日本".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.visible, vec![1]);
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("occurrences ↓ · 1/2"));
        // Each ideograph occupies two terminal cells, including a continuation.
        assert!(buffer.content.windows(5).any(|cells| {
            cells[0].symbol() == "日" && cells[2].symbol() == "本" && cells[4].symbol() == "語"
        }));
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.query, "日");
        press(&mut app, KeyCode::Enter);
        assert!(!app.searching);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.visible, vec![0, 1]);
        assert_eq!(app.group_index(), Some(1));
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.visible.is_empty());
        press(&mut app, KeyCode::Esc);
        assert!(!app.searching);
        assert!(app.query.is_empty());
    }

    #[test]
    fn focus_scrolling_occurrences_and_resize() {
        let mut data = report();
        set_source(
            &mut data.groups[0].occurrences[0],
            &"let x = 1;\n".repeat(100),
        );
        let mut app = App::new(data, ThemeName::Dracula);
        render(&mut app, 100, 24);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Focus::Code);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.scroll, 1);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.scroll, 1 + app.page);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.scroll, app.max_scroll());
        render(&mut app, 140, 60);
        assert_eq!(app.scroll, app.max_scroll());
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.scroll, 0);
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.occurrence, 1);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.occurrence, 0);
        assert_eq!(press(&mut app, KeyCode::Enter), Action::Open);
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.group_index(), Some(1));
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn empty_filtered_narrow_and_help_screens() {
        let mut app = App::new(report(), ThemeName::Dracula);
        for (w, h) in [(1, 1), (20, 6), (59, 20), (60, 12), (80, 24), (120, 40)] {
            render(&mut app, w, h);
        }
        assert!(render(&mut app, 59, 10).0.contains("Enlarge terminal"));
        press(&mut app, KeyCode::Char('?'));
        assert!(render(&mut app, 100, 30).0.contains("Browser controls"));
        press(&mut app, KeyCode::Esc);
        assert!(!app.help);
        app.query = "not found".into();
        app.filter();
        assert!(render(&mut app, 100, 24).0.contains("No matches for"));
        assert_eq!(press(&mut app, KeyCode::Enter), Action::Draw);
        let mut empty = App::new(
            BrowserReport {
                mode: "syntax".into(),
                groups: vec![],
                files: 0,
            },
            ThemeName::Dracula,
        );
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('G'),
            KeyCode::Char('n'),
            KeyCode::PageDown,
        ] {
            press(&mut empty, code);
        }
        assert!(render(&mut empty, 100, 24).0.contains("No matches in"));
        assert_eq!(
            empty.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn editor_errors_resume_before_being_reported() {
        use std::cell::RefCell;
        struct Fake<'a> {
            calls: &'a RefCell<Vec<&'static str>>,
            fail_suspend: bool,
            fail_resume: bool,
        }
        impl Suspend for Fake<'_> {
            fn suspend(&mut self) -> Result<()> {
                self.calls.borrow_mut().push("suspend");
                anyhow::ensure!(!self.fail_suspend, "suspend failed");
                Ok(())
            }
            fn resume(&mut self) -> Result<()> {
                self.calls.borrow_mut().push("resume");
                anyhow::ensure!(!self.fail_resume, "resume failed");
                Ok(())
            }
        }
        let calls = RefCell::new(Vec::new());
        let mut session = Fake {
            calls: &calls,
            fail_suspend: false,
            fail_resume: false,
        };
        let result = with_suspended(&mut session, || {
            calls.borrow_mut().push("open");
            anyhow::bail!("editor failed")
        })
        .unwrap();
        assert!(result.unwrap_err().to_string().contains("editor failed"));
        assert_eq!(*calls.borrow(), ["suspend", "open", "resume"]);
        calls.borrow_mut().clear();
        assert!(
            with_suspended(&mut session, || {
                calls.borrow_mut().push("open");
                Ok(())
            })
            .unwrap()
            .is_ok()
        );
        assert_eq!(*calls.borrow(), ["suspend", "open", "resume"]);
        session.fail_resume = true;
        assert!(with_suspended(&mut session, || Ok(())).is_err());
        session.fail_suspend = true;
        assert!(
            with_suspended(&mut session, || panic!(
                "must not open when suspension fails"
            ))
            .is_err()
        );

        let mut app = App::new(report(), ThemeName::Dracula);
        app.status = Some((true, "Editor: editor failed".into()));
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("Editor: editor failed"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.fg == dracula().error.fg.unwrap())
        );
    }

    #[test]
    fn backend_resize_and_focused_occurrence_paging() {
        let mut app = App::new(report(), ThemeName::Dracula);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        for (width, height) in [(120, 40), (60, 12), (20, 6), (1, 1), (100, 30)] {
            terminal.backend_mut().resize(width, height);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert_eq!(terminal.backend().buffer().area.width, width);
        }
        press(&mut app, KeyCode::Char('n'));
        let (small, _) = render(&mut app, 100, 24);
        assert!(small.contains("#2 parse_again"));
        assert!(!small.contains("#1 parse"));
        let (tall, _) = render(&mut app, 100, 40);
        assert!(tall.contains("#1 parse"));
        assert!(tall.contains("#2 parse_again"));
    }

    #[test]
    fn evidence_is_local_to_occurrence_and_empty_groups_are_valid() {
        let mut data = report();
        data.groups[0]
            .occurrences
            .push(snippet("third", "src/three.rs"));
        data.groups[0].edges.push(PairEvidence {
            left: 1,
            right: 2,
            cosine: 0.7,
            exact_body: false,
        });
        let mut app = App::new(data, ThemeName::Dracula);
        assert!(!evidence(&app, 0, 0).contains("#3"));
        assert!(evidence(&app, 0, 1).contains("#1 ↔ #2: same-token"));
        app.cycle(true);
        assert!(evidence(&app, 0, 1).contains("#2 ↔ #3: cosine 0.700"));
        assert!(!evidence(&app, 0, 2).contains("same-token"));
        app.report.groups[0].occurrences.clear();
        assert!(render(&mut app, 100, 24).0.contains("no occurrences"));
        assert_eq!(press(&mut app, KeyCode::Enter), Action::Draw);
    }
}
