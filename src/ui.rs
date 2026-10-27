//! The picker: the ordered list, a live preview of the selected pane, and a
//! fuzzy filter. It reads the list and calls commands like any front end,
//! and redraws when the event socket reports a change.

use std::io::BufRead;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver};
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

use tracon::client::Client;
use tracon::format::{self, Colour, Template};
use tracon::list::Entry;
use tracon::protocol::{self, Env, SettingsResult};
use tracon::runtime::Runtime;
use tracon::{Config, Locator};

/// The longest the picker waits for a key before checking for changes.
const TICK: Duration = Duration::from_millis(100);
/// Refreshes ages and catches changes made without the signal.
const LIST_INTERVAL: Duration = Duration::from_secs(2);

/// The row format the daemon's settings give, which it has checked; the
/// default stands in for one from a newer daemon this build can't read.
pub fn row_format(settings: &SettingsResult) -> Template {
    Template::parse(&settings.format).unwrap_or_else(|_| Config::default().format)
}

fn style_of(style: &format::Style) -> Style {
    let colour = |colour: Colour| match colour {
        Colour::Indexed(index) => Color::Indexed(index),
        Colour::Rgb(r, g, b) => Color::Rgb(r, g, b),
    };
    let mut drawn = Style::new();
    if let Some(fg) = style.fg {
        drawn = drawn.fg(colour(fg));
    }
    if let Some(bg) = style.bg {
        drawn = drawn.bg(colour(bg));
    }
    for (set, modifier) in [
        (style.bold, Modifier::BOLD),
        (style.dim, Modifier::DIM),
        (style.italic, Modifier::ITALIC),
        (style.underline, Modifier::UNDERLINED),
        (style.reversed, Modifier::REVERSED),
    ] {
        if set {
            drawn = drawn.add_modifier(modifier);
        }
    }
    drawn
}

fn row(entry: &Entry, format: &Template, now: u64) -> ListItem<'static> {
    let spans: Vec<Span<'static>> = entry
        .row(format, now)
        .into_iter()
        .map(|span| Span::styled(span.text, style_of(&span.style)))
        .collect();
    ListItem::new(Line::from(spans))
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

/// Reads the event socket on a thread and reports each message; the
/// picker reloads the list on any. Dropping it closes the socket.
struct Events {
    stream: Option<UnixStream>,
    signals: Receiver<()>,
}

impl Events {
    fn start(runtime: &Runtime) -> Self {
        let (sender, signals) = mpsc::channel();
        let Ok(reader) = Client::subscribe(runtime) else {
            return Events {
                stream: None,
                signals,
            };
        };
        let stream = reader.get_ref().try_clone().ok();
        thread::spawn(move || {
            for line in reader.lines() {
                if line.is_err() || sender.send(()).is_err() {
                    return;
                }
            }
        });
        Events { stream, signals }
    }

    fn signalled(&self) -> bool {
        let mut any = false;
        while self.signals.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

struct Picker<'a> {
    rpc: Client,
    env: Env,
    preview_interval: Duration,
    format: Template,
    client: Option<&'a str>,
    entries: Vec<Entry>,
    /// Indices into `entries` that match the filter, in list order.
    visible: Vec<usize>,
    filter: String,
    /// Keys edit the filter rather than act on the list.
    filtering: bool,
    list: ListState,
    preview: Text<'static>,
    preview_pane: Option<Locator>,
    status: Option<String>,
    matcher: Matcher,
}

impl Picker<'_> {
    fn reload(&mut self) -> Result<()> {
        let selected = self.selected().map(|entry| entry.pane.clone());
        let params = protocol::List {
            env: self.env.clone(),
        };
        self.entries = self.rpc.call("list", params)?;
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
                    "{} {} {} {} {} {}",
                    entry.location,
                    entry.label,
                    entry.status.as_ref().map_or("", |status| status.as_str()),
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
        let params = protocol::Capture {
            pane: pane.clone(),
            plain: false,
        };
        let capture: String = self.rpc.call("capture", params).unwrap_or_default();
        let capture = trim_capture(&capture);
        self.preview = capture
            .as_bytes()
            .into_text()
            .unwrap_or_else(|_| Text::raw(capture));
        self.preview_pane = Some(pane);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let list_height = (self.visible.len() as u16 + 2)
            .min(area.height.saturating_mul(2) / 5)
            .max(3);
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
            .map(|&i| row(&self.entries[i], &self.format, now))
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

        let hint = self.status.clone().unwrap_or_else(|| {
            if self.filtering {
                "enter focus · esc clear"
            } else {
                "enter focus · p pin · / filter · esc close"
            }
            .to_owned()
        });
        let input = if self.filtering {
            Line::from(vec![
                Span::raw("/").bold(),
                Span::raw(self.filter.clone()),
                Span::raw("  "),
                Span::raw(hint).dim(),
            ])
        } else {
            Line::from(Span::raw(hint).dim())
        };
        frame.render_widget(Paragraph::new(input), input_area);
        if self.filtering {
            frame.set_cursor_position((
                input_area.x + 1 + self.filter.chars().count() as u16,
                input_area.y,
            ));
        }
    }

    /// Handle a key, returning whether the picker should close: a pane was
    /// focused or the picker was dismissed.
    fn key(&mut self, key: KeyEvent) -> Result<bool> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c' | 'g') if control => return Ok(true),
            KeyCode::Enter => {
                if let Some(entry) = self.selected() {
                    let params = protocol::Focus {
                        pane: entry.pane.to_string(),
                        client: self.client.map(str::to_owned),
                        env: self.env.clone(),
                    };
                    match self.rpc.call::<()>("focus", params) {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.status = Some(error.to_string());
                            self.reload()?;
                        }
                    }
                }
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('p' | 'k') if control => self.move_selection(-1),
            KeyCode::Char('n' | 'j') if control => self.move_selection(1),
            _ if self.filtering => self.filter_key(key.code, control),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('p') => {
                if let Some(entry) = self.selected() {
                    let params = protocol::Pin {
                        pane: Some(entry.pane.to_string()),
                        pinned: !entry.pinned,
                        env: self.env.clone(),
                    };
                    self.rpc.call::<()>("pin", params)?;
                    self.reload()?;
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            _ => {}
        }
        Ok(false)
    }

    fn filter_key(&mut self, code: KeyCode, control: bool) {
        match code {
            // Back to the whole list, still on the chosen pane.
            KeyCode::Esc => {
                let selected = self.selected().map(|entry| entry.pane.clone());
                self.filtering = false;
                self.filter.clear();
                self.refilter(selected);
            }
            KeyCode::Backspace if self.filter.is_empty() => self.filtering = false,
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
    }
}

pub fn pick(runtime: &Runtime, client: Option<&str>) -> Result<()> {
    let env = protocol::env();
    let mut rpc = Client::connect(runtime)?;
    let settings: SettingsResult = rpc.call("settings", protocol::Settings { env: env.clone() })?;
    let mut picker = Picker {
        rpc,
        env,
        preview_interval: Duration::from_millis(settings.preview_interval_ms),
        format: row_format(&settings),
        client,
        entries: Vec::new(),
        visible: Vec::new(),
        filter: String::new(),
        filtering: false,
        list: ListState::default(),
        preview: Text::default(),
        preview_pane: None,
        status: None,
        matcher: Matcher::new(nucleo_matcher::Config::DEFAULT),
    };
    picker.reload()?;
    picker.refresh_preview();
    let events = Events::start(runtime);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut picker, &events);
    ratatui::restore();
    drop(events);
    result
}

fn event_loop(terminal: &mut DefaultTerminal, picker: &mut Picker, events: &Events) -> Result<()> {
    let mut last_list = Instant::now();
    let mut last_preview = Instant::now();
    let preview_interval = picker.preview_interval;
    loop {
        terminal.draw(|frame| picker.draw(frame))?;
        let wait = TICK.min(preview_interval.saturating_sub(last_preview.elapsed()));
        if event::poll(wait)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && picker.key(key)?
        {
            return Ok(());
        }
        if events.signalled() || last_list.elapsed() >= LIST_INTERVAL {
            picker.reload()?;
            last_list = Instant::now();
        }
        let moved = picker.selected().map(|entry| &entry.pane) != picker.preview_pane.as_ref();
        if moved || last_preview.elapsed() >= preview_interval {
            picker.refresh_preview();
            last_preview = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_drawn_as_given() {
        let style = format::Style::parse("bold bg:blue #ff0000").unwrap();
        let drawn = style_of(&style);
        assert_eq!(drawn.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(drawn.bg, Some(Color::Indexed(4)));
        assert!(drawn.add_modifier.contains(Modifier::BOLD));
        assert!(!drawn.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn captures_lose_trailing_blank_lines() {
        assert_eq!(trim_capture("a\n b\n\n  \u{1b}[0m \n\n"), "a\n b");
        assert!(!blank("\u{1b}[1mx"));
    }
}
