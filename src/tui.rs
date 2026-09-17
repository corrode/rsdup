//! Interactive, read-only match browser. Source is tokenized once, never during drawing.

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
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::{
    snippet_diff::{LineChange, SnippetDiff},
    tui_data::{BrowserReport, Snippet},
};

const BG: Color = Color::Rgb(18, 22, 31);
const PANEL: Color = Color::Rgb(25, 30, 41);
const FG: Color = Color::Rgb(218, 226, 239);
const MUTED: Color = Color::Rgb(143, 157, 180);
const BORDER: Color = Color::Rgb(64, 76, 96);
const ACCENT: Color = Color::Rgb(105, 211, 200);
const KEYWORD: Color = Color::Rgb(192, 156, 245);
const STRING: Color = Color::Rgb(166, 211, 139);
const NUMBER: Color = Color::Rgb(240, 183, 126);
const FUNCTION: Color = Color::Rgb(128, 190, 247);
const ERROR: Color = Color::Rgb(245, 151, 150);
const REMOVED_BG: Color = Color::Rgb(65, 35, 43);
const ADDED_BG: Color = Color::Rgb(29, 57, 45);
const CHANGED_BG: Color = Color::Rgb(61, 51, 32);

type Highlighted = Vec<Line<'static>>;

/// Browse cached source context without reading files or modifying the report.
pub(crate) fn run(report: BrowserReport, editor: Option<&str>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "the match browser requires an interactive terminal on stdin and stdout; run without piping or redirection, or use a non-interactive output mode"
        );
    }
    let mut app = App::new(report);

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

struct App {
    diff_enabled: bool,
    pair_diff: Option<PairDiff>,
    report: BrowserReport,
    highlighted: Vec<Vec<Arc<Highlighted>>>,
    search_text: Vec<String>,
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
    fn new(report: BrowserReport) -> Self {
        let mut cache: HashMap<Arc<str>, Arc<Highlighted>> = HashMap::new();
        let highlighted = report
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
                                .or_insert_with(|| Arc::new(highlight(&snippet.display_source))),
                        )
                    })
                    .collect()
            })
            .collect();
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
        let visible = (0..report.groups.len()).collect();
        let mut app = Self {
            diff_enabled: true,
            pair_diff: None,
            report,
            highlighted,
            search_text,
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
        app.reset_scroll();
        app.refresh_diff();
        app
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
            KeyCode::Char('d') => self.diff_enabled = !self.diff_enabled,
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

fn highlight(source: &str) -> Highlighted {
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
            MUTED
        } else if matches!(
            kind,
            SyntaxKind::STRING
                | SyntaxKind::BYTE_STRING
                | SyntaxKind::C_STRING
                | SyntaxKind::CHAR
                | SyntaxKind::BYTE
        ) {
            STRING
        } else if matches!(kind, SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER) {
            NUMBER
        } else if kind.is_keyword(Edition::CURRENT) {
            KEYWORD
        } else if kind == SyntaxKind::IDENT
            && tokens[index + 1..]
                .iter()
                .find(|next| !matches!(next.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
                .is_some_and(|next| next.text() == "(")
        {
            FUNCTION
        } else {
            FG
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

fn panel(title: impl Into<Line<'static>>, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if active { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(FG))
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if area.width < 60 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("rsdup · match browser", Style::default().fg(ACCENT)),
                Line::from("Enlarge terminal to at least 60 × 12."),
                Line::from(format!(
                    "{} matches · {} files",
                    app.report.groups.len(),
                    app.report.files
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
                Span::styled(
                    " rsdup ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("/ {} / match browser", app.report.mode)),
            ]),
            Line::styled(
                format!(
                    " {} matches · {} files · read-only",
                    app.report.groups.len(),
                    app.report.files
                ),
                Style::default().fg(MUTED),
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
        draw_help(frame, area);
    }
}

fn sidebar_width(width: u16) -> u16 {
    (u32::from(width) * 3 / 10).clamp(20, 48) as u16
}

fn draw_groups(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<_> = app
        .visible
        .iter()
        .map(|&index| {
            let group = &app.report.groups[index];
            ListItem::new(vec![
                Line::from(format!("{:>3}  {}", index + 1, group.title)),
                Line::styled(
                    format!("     {} occurrences", group.occurrences.len()),
                    Style::default().fg(MUTED),
                ),
            ])
        })
        .collect();
    app.list
        .select((!app.visible.is_empty()).then_some(app.selected));
    let block = panel(
        format!(
            " Matches {}/{} ",
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
                .highlight_style(
                    Style::default()
                        .bg(Color::Rgb(42, 66, 78))
                        .fg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸"),
            area,
            &mut app.list,
        );
    }
}

fn draw_code(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(group_index) = app.group_index() else {
        frame.render_widget(
            Paragraph::new("Select a match, or press / to search.").block(panel(" Source ", false)),
            area,
        );
        return;
    };
    let group = &app.report.groups[group_index];
    let count = group.occurrences.len();
    if count == 0 {
        frame.render_widget(
            Paragraph::new("This group has no occurrences.").block(panel(" Source ", false)),
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
                    " {} · occurrence {}/{}",
                    group.title,
                    app.occurrence + 1,
                    count
                ),
                Style::default().fg(ACCENT),
            ),
            Line::styled(
                format!(" {}", group.description),
                Style::default().fg(MUTED),
            ),
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
            .style(Style::default().fg(FUNCTION)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(evidence(app, group, occurrence)).style(Style::default().fg(ACCENT)),
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
        Paragraph::new(info).style(Style::default().fg(if snippet.warning.is_some() {
            NUMBER
        } else {
            MUTED
        })),
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
            LineChange::Equal => ('│', PANEL),
            LineChange::Removed => ('−', REMOVED_BG),
            LineChange::Added => ('+', ADDED_BG),
            LineChange::Changed => (changes.map_or('│', |(_, marker)| marker), CHANGED_BG),
        }
    };
    let numbers: Vec<_> = visible
        .clone()
        .map(|(index, _)| {
            let (marker, bg) = decoration(index);
            Line::styled(
                format!(
                    "{:>digits$} {marker} ",
                    snippet.display_start_line.saturating_add(index)
                ),
                Style::default().bg(bg),
            )
        })
        .collect();
    let source: Vec<_> = visible
        .map(|(index, line)| {
            let mut line = line.clone();
            line.style = line.style.bg(decoration(index).1);
            // Context is line-based: boundary lines retain syntax even when the
            // matched item starts or ends mid-line. Override token colors too.
            if match_index(index).is_none() {
                line.style = line.style.fg(MUTED);
                for span in &mut line.spans {
                    span.style = span.style.fg(MUTED);
                }
            }
            line
        })
        .collect();
    frame.render_widget(
        Paragraph::new(numbers).style(Style::default().fg(MUTED)),
        gutter,
    );
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
        let mut style = Style::default().bg(decoration(index).1);
        if match_index(index).is_none() {
            style = style.fg(MUTED);
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
                Style::default().fg(
                    if app.status.as_ref().is_some_and(|(error, _)| *error) && !app.searching {
                        ERROR
                    } else {
                        ACCENT
                    },
                ),
            ),
            Line::from(diff_legend(app)),
            Line::styled(
                " Tab focus · n/p pair · H/L pan · Enter open · ? help · q quit",
                Style::default().fg(MUTED),
            ),
        ]),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = area.width.min(68);
    let height = area.height.min(20);
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
            Line::from("PgUp / PgDn         Page in the focused pane"),
            Line::from("g/G / Home/End      First / last match or source page"),
            Line::from("Context dimmed by line; matched lines keep syntax and diff."),
            Line::from("/                   Search titles, names and paths"),
            Line::from("Enter in search     Keep filter; return to navigation"),
            Line::from("Esc                 Clear filter / cancel search"),
            Line::from("Enter / o           Open focused occurrence in editor"),
            Line::from("q / Ctrl-C          Quit (Ctrl-C also during search)"),
            Line::from("Amber ±: changed; LIMIT: conservative marks (work cap)"),
            Line::styled(
                "Pair evidence is not a semantic equivalence claim.",
                Style::default().fg(MUTED),
            ),
            Line::from("Esc / ? / Enter     Close help"),
        ])
        .block(panel(" Browser controls ", true)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_data::{MatchGroup, PairEvidence};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

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
        let mut app = App::new(context_report());
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
            assert!(row[code..=end].iter().all(|c| c.fg == MUTED));
            assert!(
                row[code..=end].iter().all(|c| c.bg == PANEL),
                "context line {n}: {:?}",
                &row[code..=end]
            );
        }
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.fg == KEYWORD && c.bg == CHANGED_BG)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.fg == NUMBER && c.bg == CHANGED_BG)
        );
        // Dimming visible context must not mutate the shared syntax cache.
        assert_eq!(app.highlighted[0][0][6].spans[0].style.fg, Some(KEYWORD));
        assert_eq!(app.scroll, 6);
    }

    #[test]
    fn source_scrolls_beyond_match_and_navigation_refocuses_each_occurrence() {
        let mut app = App::new(context_report());
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
                .all(|c| !matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
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
        let mut app = App::new(context_report());
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
            let mut app = App::new(context_report());
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
        let mut app = App::new(data);
        assert!(Arc::ptr_eq(&app.highlighted[0][0], &app.highlighted[0][1]));
        let cache = Arc::clone(&app.highlighted[0][0]);
        std::fs::write(&path, "changed on disk").unwrap();
        assert!(render(&mut app, 120, 24).0.contains("fn line_10"));
        std::fs::remove_file(&path).unwrap();
        app.cycle(true);
        assert!(render(&mut app, 160, 48).0.contains("fn line_30"));
        assert!(Arc::ptr_eq(&cache, &app.highlighted[0][0]));
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
            let mut app = App::new(data);
            let (text, buffer) = render(&mut app, 120, 24);
            assert!(text.contains(&format!("WARNING: {warning}")));
            assert!(text.contains(" 42 │ fn parse()"));
            assert!(!text.contains("unrelated disk content"));
            assert!(buffer.content.iter().any(|c| c.fg == KEYWORD));
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
        let mut app = App::new(data);
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
            assert_eq!(row[gutter + 1].fg, MUTED);
            assert_eq!(row[gutter + 6].fg, FG);
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
        App::new(data)
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
                    .find(|c| matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
                    .map_or(PANEL, |c| c.bg);
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
                .all(|(_, bg)| *bg == PANEL)
        );
        assert_eq!(source_rows(&buffer, "fn inserted()")[0].1, ADDED_BG);
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.bg == ADDED_BG && c.fg == KEYWORD)
        );
        press(&mut app, KeyCode::Char('d'));
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("d OFF"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
        );
        press(&mut app, KeyCode::Char('d'));
        assert!(
            render(&mut app, 120, 40)
                .1
                .content
                .iter()
                .any(|c| c.bg == ADDED_BG)
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
                .any(|c| c.bg == CHANGED_BG && c.fg == STRING)
        );
        app.focus = Focus::Code;
        app.horizontal = 4;
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("42 − 日"));
        assert!(
            buffer
                .content
                .iter()
                .any(|c| c.symbol() == "日" && c.bg == CHANGED_BG)
        );
        let mut equal = diff_app("let 日本語 = 42;\n", "let 日本語 = 42;\n");
        assert!(
            render(&mut equal, 120, 40)
                .1
                .content
                .iter()
                .all(|c| !matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
        );
    }

    #[test]
    fn actual_pair_wraps_and_remains_explicit_in_single_card() {
        let mut app = diff_app("a\n", "b\n");
        let mut third = snippet("third", "src/third.rs");
        set_source(&mut third, "b\n");
        let mut data = app.report;
        data.groups[0].occurrences.push(third);
        app = App::new(data);
        app.cycle(true);
        let (text, buffer) = render(&mut app, 120, 40);
        assert!(text.contains("compared #2 ↔ #3"));
        assert!(text.contains("No pair similarity: #2 ↔ #3 · no direct edge"));
        assert!(
            buffer
                .content
                .iter()
                .all(|c| !matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
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
                .all(|c| !matches!(c.bg, ADDED_BG | REMOVED_BG | CHANGED_BG))
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
        let mut app = App::new(data);
        press(&mut app, KeyCode::End);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 2);
        let (_, buffer) = render(&mut app, 120, 40);
        assert_eq!(source_rows(&buffer, "fn removed()")[0].1, REMOVED_BG);
        assert!(
            source_rows(&buffer, "fn stay()")
                .iter()
                .all(|(_, bg)| *bg == PANEL)
        );
        press(&mut app, KeyCode::Home);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 0);
        app.page = 2;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 2);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.pair_diff.as_ref().unwrap().group, 0);
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
        assert!(buffer.content.iter().any(|c| c.bg == CHANGED_BG));
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
        let mut app = App::new(data);
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

    #[test]
    fn sidebar_width_does_not_overflow() {
        assert_eq!(sidebar_width(60), 20);
        assert_eq!(sidebar_width(100), 30);
        assert_eq!(sidebar_width(u16::MAX), 48);
    }

    #[test]
    fn browser_snapshot_headings_pair_source_and_colors() {
        let mut app = App::new(report());
        let (text, buffer) = render(&mut app, 120, 40);
        for expected in [
            "rsdup",
            "Matches 2/2",
            "Duplicate parsers",
            "#1 parse",
            "#2 parse_again",
            "src/one.rs:42:5",
            "42 │ fn parse()",
            "same-token",
            "cosine 0.988",
            "Tab focus",
        ] {
            assert!(text.contains(expected), "missing {expected:?}\n{text}");
        }
        for color in [KEYWORD, STRING, NUMBER, FUNCTION] {
            assert!(
                buffer.content.iter().any(|cell| cell.fg == color),
                "missing {color:?}"
            );
        }
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == Color::Rgb(42, 66, 78))
        );
        assert!(!text.contains("equivalence"));
    }

    #[test]
    fn multiline_tokens_keep_styles_and_original_text() {
        let source = "/* a\nb */\nlet s = r#\"a\nb\"#;\n";
        let lines = highlight(source);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].spans[0].style.fg, Some(MUTED));
        assert_eq!(lines[3].spans[0].style.fg, Some(STRING));
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
        let mut app = App::new(report());
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.group_index(), Some(1));
        press(&mut app, KeyCode::Char('/'));
        for c in "日本".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.visible, vec![1]);
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("Matches 1/2"));
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
        let mut app = App::new(data);
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
        let mut app = App::new(report());
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
        let mut empty = App::new(BrowserReport {
            mode: "syntax".into(),
            groups: vec![],
            files: 0,
        });
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

        let mut app = App::new(report());
        app.status = Some((true, "Editor: editor failed".into()));
        let (text, buffer) = render(&mut app, 100, 24);
        assert!(text.contains("Editor: editor failed"));
        assert!(buffer.content.iter().any(|cell| cell.fg == ERROR));
    }

    #[test]
    fn backend_resize_and_focused_occurrence_paging() {
        let mut app = App::new(report());
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
        let mut app = App::new(data);
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
