//! The picker: the ordered list, a live preview of the selected pane, and a
//! fuzzy filter. It reads the list and calls commands like any front end.

use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use tracon::backend::{Backend, TmuxBackend};
use tracon::list::{self, Entry};
use tracon::store::{Store, TmuxStore};
use tracon::tmux::{CHANNEL, Tmux};
use tracon::{Config, State, harness};

const PREVIEW_INTERVAL: Duration = Duration::from_millis(500);
/// Refreshes ages and catches changes made without the signal.
const LIST_INTERVAL: Duration = Duration::from_secs(2);

/// How long ago `since` was, such as `42s`, `5m` or `3h`.
pub fn age(since: Option<u64>, now: u64) -> String {
    let Some(since) = since else {
        return String::new();
    };
    let seconds = now.saturating_sub(since);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn glyph(state: Option<&State>) -> (&'static str, Style) {
    let root = state.map_or("", State::root);
    match root {
        "waiting" => (
            "●",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        "error" => (
            "✗",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        "done" => ("✓", Style::new().fg(Color::Green)),
        "working" => ("◐", Style::new().fg(Color::Blue)),
        "idle" => ("○", Style::new().fg(Color::Gray)),
        "" => (" ", Style::new()),
        _ => ("?", Style::new().fg(Color::DarkGray)),
    }
}

/// The message, or the pane title when there is none.
fn summary(entry: &Entry) -> &str {
    entry
        .message
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(&entry.title)
}

/// One line per pane for `tracon list`.
pub fn plain_row(entry: &Entry, now: u64) -> String {
    format!(
        "{}{} {:<18} {:>4}  {:<16} {:<7} {}",
        if entry.pinned { "*" } else { " " },
        glyph(entry.state.as_ref()).0,
        entry.state.as_ref().map_or("-", State::as_str),
        age(entry.since, now),
        entry.location,
        entry.harness.as_deref().unwrap_or(""),
        summary(entry),
    )
}

fn row(entry: &Entry, now: u64) -> ListItem<'static> {
    let (glyph, style) = glyph(entry.state.as_ref());
    ListItem::new(Line::from(vec![
        Span::raw(if entry.pinned { "*" } else { " " }),
        Span::styled(glyph, style),
        Span::raw(" "),
        Span::styled(
            format!("{:<18}", entry.state.as_ref().map_or("-", State::as_str)),
            style,
        ),
        Span::raw(format!("{:>4}  ", age(entry.since, now))),
        Span::raw(format!("{:<16} ", entry.location)).bold(),
        Span::raw(format!("{:<7} ", entry.harness.as_deref().unwrap_or(""))).dim(),
        Span::raw(summary(entry).to_owned()),
    ]))
}

/// Whether a captured line shows nothing: only spaces and escape sequences.
fn blank(line: &str) -> bool {
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI sequences end with a byte in @..~.
                if chars.next() == Some('[') {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            c if c.is_whitespace() => {}
            _ => return false,
        }
    }
    true
}

/// The capture with trailing blank lines dropped.
fn trim_capture(capture: &str) -> String {
    let lines: Vec<&str> = capture.lines().collect();
    let end = lines
        .iter()
        .rposition(|line| !blank(line))
        .map_or(0, |last| last + 1);
    lines[..end].join("\n")
}

/// Runs `tmux wait-for tracon` in a loop and reports each signal. Dropping
/// the watcher kills the waiting tmux client, which would otherwise outlive
/// the picker until the next signal.
struct Watcher {
    waiting: Arc<Mutex<Waiting>>,
    signals: Receiver<()>,
}

#[derive(Default)]
struct Waiting {
    closed: bool,
    child: Option<Child>,
}

impl Watcher {
    fn start(tmux: &Tmux) -> Self {
        let waiting = Arc::new(Mutex::new(Waiting::default()));
        let (sender, signals) = mpsc::channel();
        let shared = Arc::clone(&waiting);
        let tmux = tmux.clone();
        thread::spawn(move || {
            loop {
                let Ok(mut child) = tmux
                    .command()
                    .args(["wait-for", CHANNEL])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    return;
                };
                match shared.lock() {
                    Ok(mut state) if !state.closed => state.child = Some(child),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                }
                // Poll rather than block in wait, so the child stays where
                // Drop can kill it.
                loop {
                    thread::sleep(Duration::from_millis(50));
                    let Ok(mut state) = shared.lock() else { return };
                    match state.child.as_mut().map(Child::try_wait) {
                        Some(Ok(Some(_))) => {
                            state.child = None;
                            break;
                        }
                        Some(Ok(None)) => {}
                        _ => return,
                    }
                }
                if sender.send(()).is_err() {
                    return;
                }
            }
        });
        Watcher { waiting, signals }
    }

    fn signalled(&self) -> bool {
        let mut any = false;
        while self.signals.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Ok(mut state) = self.waiting.lock() {
            state.closed = true;
            if let Some(mut child) = state.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

struct Picker<'a> {
    backend: &'a TmuxBackend,
    store: &'a TmuxStore,
    config: &'a Config,
    client: Option<&'a str>,
    entries: Vec<Entry>,
    /// Indices into `entries` that match the filter, in list order.
    visible: Vec<usize>,
    filter: String,
    list: ListState,
    preview: Text<'static>,
    preview_pane: Option<String>,
    status: Option<String>,
    matcher: Matcher,
}

impl Picker<'_> {
    fn reload(&mut self) -> Result<()> {
        let selected = self.selected().map(|entry| entry.pane.clone());
        let records = self.store.load()?;
        let panes = self.backend.panes()?;
        self.entries = list::entries(panes, records, &harness::all(), &self.config.order);
        self.refilter(selected);
        Ok(())
    }

    fn refilter(&mut self, keep: Option<tracon::Locator>) {
        let pattern = Pattern::parse(&self.filter, CaseMatching::Ignore, Normalization::Smart);
        let mut buffer = Vec::new();
        self.visible = (0..self.entries.len())
            .filter(|&index| {
                let entry = &self.entries[index];
                let haystack = format!(
                    "{} {} {} {} {}",
                    entry.location,
                    entry.state.as_ref().map_or("", State::as_str),
                    entry.harness.as_deref().unwrap_or(""),
                    entry.title,
                    entry.message.as_deref().unwrap_or("")
                );
                pattern
                    .score(Utf32Str::new(&haystack, &mut buffer), &mut self.matcher)
                    .is_some()
            })
            .collect();
        let position = keep
            .and_then(|pane| {
                self.visible
                    .iter()
                    .position(|&i| self.entries[i].pane == pane)
            })
            .or((!self.visible.is_empty()).then_some(0));
        self.list.select(position);
    }

    fn selected(&self) -> Option<&Entry> {
        let index = *self.visible.get(self.list.selected()?)?;
        self.entries.get(index)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() as isize - 1;
        let current = self.list.selected().unwrap_or(0) as isize;
        self.list
            .select(Some((current + delta).clamp(0, last) as usize));
    }

    fn refresh_preview(&mut self) {
        let Some(pane) = self.selected().map(|entry| entry.pane.clone()) else {
            self.preview = Text::default();
            self.preview_pane = None;
            return;
        };
        let capture = self.backend.capture(&pane).unwrap_or_default();
        let capture = trim_capture(&capture);
        self.preview = capture
            .as_bytes()
            .into_text()
            .unwrap_or_else(|_| Text::raw(capture));
        self.preview_pane = Some(pane.id);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let list_height =
            (self.visible.len() as u16 + 2).clamp(3, area.height.saturating_mul(2) / 5);
        let [list_area, preview_area, input_area] = Layout::vertical([
            Constraint::Length(list_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let now = tracon::now();
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| row(&self.entries[i], now))
            .collect();
        let list = List::new(items)
            .block(
                Block::new()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::new().dim()),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_area, &mut self.list);

        let title = self
            .selected()
            .map(|entry| format!(" {} {} ", entry.location, entry.title))
            .unwrap_or_default();
        let inner_height = preview_area.height.saturating_sub(1) as usize;
        // Keep the bottom of the screen, where agents prompt, in view.
        let scroll = self.preview.lines.len().saturating_sub(inner_height) as u16;
        let preview = Paragraph::new(self.preview.clone())
            .block(
                Block::new()
                    .borders(Borders::TOP)
                    .title(title)
                    .border_style(Style::new().dim()),
            )
            .scroll((scroll, 0));
        frame.render_widget(preview, preview_area);

        let hint = self
            .status
            .clone()
            .unwrap_or_else(|| "enter focus · tab pin · esc close".to_owned());
        let input = Line::from(vec![
            Span::raw("> ").bold(),
            Span::raw(self.filter.clone()),
            Span::raw("  "),
            Span::raw(hint).dim(),
        ]);
        frame.render_widget(Paragraph::new(input), input_area);
        frame.set_cursor_position((
            input_area.x + 2 + self.filter.chars().count() as u16,
            input_area.y,
        ));
    }

    /// Handle a key, returning whether the picker should close: a pane was
    /// focused or the picker was dismissed.
    fn key(&mut self, key: KeyEvent) -> Result<bool> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(true),
            KeyCode::Char('c' | 'g') if control => return Ok(true),
            KeyCode::Enter => {
                if let Some(entry) = self.selected() {
                    let pane = entry.pane.clone();
                    match self.backend.focus(&pane, self.client) {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.status = Some(error.to_string());
                            self.reload()?;
                        }
                    }
                }
            }
            KeyCode::Tab => {
                if let Some(entry) = self.selected() {
                    let (pane, pinned) = (entry.pane.clone(), entry.pinned);
                    self.store.set_pinned(&pane, !pinned)?;
                    self.reload()?;
                }
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('p' | 'k') if control => self.move_selection(-1),
            KeyCode::Char('n' | 'j') if control => self.move_selection(1),
            KeyCode::Char('u') if control => {
                self.filter.clear();
                self.refilter(None);
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter(None);
            }
            KeyCode::Char(c) if !control => {
                self.filter.push(c);
                self.refilter(None);
            }
            _ => {}
        }
        Ok(false)
    }
}

pub fn pick(
    tmux: &Tmux,
    backend: &TmuxBackend,
    store: &TmuxStore,
    config: &Config,
    client: Option<&str>,
) -> Result<()> {
    let mut picker = Picker {
        backend,
        store,
        config,
        client,
        entries: Vec::new(),
        visible: Vec::new(),
        filter: String::new(),
        list: ListState::default(),
        preview: Text::default(),
        preview_pane: None,
        status: None,
        matcher: Matcher::new(nucleo_matcher::Config::DEFAULT),
    };
    picker.reload()?;
    picker.refresh_preview();
    let watcher = Watcher::start(tmux);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut picker, &watcher);
    ratatui::restore();
    drop(watcher);
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    picker: &mut Picker,
    watcher: &Watcher,
) -> Result<()> {
    let mut last_list = Instant::now();
    let mut last_preview = Instant::now();
    loop {
        terminal.draw(|frame| picker.draw(frame))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && picker.key(key)?
        {
            return Ok(());
        }
        if watcher.signalled() || last_list.elapsed() >= LIST_INTERVAL {
            picker.reload()?;
            last_list = Instant::now();
        }
        let moved =
            picker.selected().map(|entry| entry.pane.id.as_str()) != picker.preview_pane.as_deref();
        if moved || last_preview.elapsed() >= PREVIEW_INTERVAL {
            picker.refresh_preview();
            last_preview = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(age(Some(100), 142), "42s");
        assert_eq!(age(Some(0), 300), "5m");
        assert_eq!(age(Some(0), 3 * 3600 + 5), "3h");
        assert_eq!(age(Some(0), 2 * 86400), "2d");
        assert_eq!(age(Some(10), 5), "0s");
        assert_eq!(age(None, 5), "");
    }

    #[test]
    fn captures_lose_trailing_blank_lines() {
        assert_eq!(trim_capture("a\n b\n\n  \u{1b}[0m \n\n"), "a\n b");
        assert!(!blank("\u{1b}[1mx"));
    }
}
