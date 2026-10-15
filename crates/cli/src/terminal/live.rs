//! One terminal owner while a model turn runs. The idle editor is inactive.
use super::change_view;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, terminal,
};
use serde_json::Value;
use std::{
    io::{self, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc as async_mpsc, oneshot};
use unicode_width::UnicodeWidthChar;

pub enum Action {
    Submit(String),
    SideQuestion(String),
    Cancel,
    Detach,
    Draft(String),
}
enum Update {
    Text(String),
    Notice(String),
    Styled(Vec<change_view::Line>),
    Activity(String),
    Usage(Value),
    Question(String, oneshot::Sender<String>),
    Detail(String, String, String),
    Thinking(String),
    Stop(Option<String>),
}
pub struct LiveTurn {
    updates: mpsc::Sender<Update>,
    pub actions: async_mpsc::UnboundedReceiver<Action>,
    worker: Option<thread::JoinHandle<()>>,
}
impl LiveTurn {
    pub fn start(
        mode: &str,
        bindings: crate::keybindings::Keybindings,
        history: Vec<String>,
    ) -> Option<Self> {
        if crate::terminal::screen_reader()
            || !io::stdin().is_terminal()
            || !io::stdout().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var("TERM").as_deref() == Ok("dumb")
        {
            return None;
        }
        let (updates, receiver) = mpsc::channel();
        let (actions, action_receiver) = async_mpsc::unbounded_channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let mode = mode.to_owned();
        let worker = thread::spawn(move || {
            let result = terminal::enable_raw_mode()
                .and_then(|_| execute!(io::stderr(), EnableBracketedPaste));
            if result.is_err() {
                let _ = terminal::disable_raw_mode();
                let _ = ready.send(false);
                return;
            }
            let _guard = RawGuard;
            let _ = ready.send(true);
            let mut state = State::new(mode);
            state.bindings = bindings;
            state.history = history;
            state.history_index = state.history.len();
            let mut screen = Frame::default();
            let _ = run(&mut state, &mut screen, receiver, actions);
            let _ = screen.clear();
        });
        if initialized.recv().ok() != Some(true) {
            let _ = worker.join();
            return None;
        }
        Some(Self {
            updates,
            actions: action_receiver,
            worker: Some(worker),
        })
    }
    pub fn detail(&self, id: &str, title: &str, body: &str) {
        let _ = self.updates.send(Update::Detail(
            id.into(),
            title.into(),
            body.chars().take(65536).collect(),
        ));
    }
    pub fn thinking(&self, text: &str) {
        let _ = self.updates.send(Update::Thinking(text.into()));
    }
    pub fn text(&self, text: &str) {
        let _ = self.updates.send(Update::Text(text.into()));
    }
    pub fn notice(&self, text: &str) {
        let _ = self.updates.send(Update::Notice(text.into()));
    }
    fn styled(&self, lines: Vec<change_view::Line>) {
        let _ = self.updates.send(Update::Styled(lines));
    }
    pub fn activity(&self, text: &str) {
        let _ = self.updates.send(Update::Activity(text.into()));
    }
    pub fn usage(&self, value: &Value) {
        let _ = self.updates.send(Update::Usage(value.clone()));
    }
    pub fn question(&self, text: String) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        let _ = self.updates.send(Update::Question(text, send));
        receive
    }
    pub fn finish(mut self, error: Option<&str>) -> Vec<Action> {
        let _ = self.updates.send(Update::Stop(error.map(str::to_owned)));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let mut pending = Vec::new();
        while let Ok(action) = self.actions.try_recv() {
            pending.push(action);
        }
        pending
    }
}
impl Drop for LiveTurn {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.updates.send(Update::Stop(Some("Interrupted".into())));
            let _ = worker.join();
        }
    }
}
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), DisableBracketedPaste, DisableMouseCapture);
        let _ = terminal::disable_raw_mode();
    }
}
#[derive(Default, Clone)]
struct Draft {
    characters: Vec<char>,
    cursor: usize,
}
impl Draft {
    fn insert(&mut self, text: &str) {
        for ch in text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        {
            if self.characters.len() >= 32_768 {
                break;
            }
            self.characters.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }
    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.characters).into_iter().collect()
    }
    fn set(&mut self, text: &str) {
        self.characters.clear();
        self.cursor = 0;
        self.insert(text);
    }
    fn visible(&self, width: usize) -> (String, usize) {
        let mut start = 0;
        while self.characters[start..self.cursor]
            .iter()
            .map(|ch| ch.width().unwrap_or(1).max(1))
            .sum::<usize>()
            >= width.max(1)
        {
            start += 1;
        }
        let cursor = self.characters[start..self.cursor]
            .iter()
            .map(|ch| ch.width().unwrap_or(1).max(1))
            .sum();
        let text = self.characters[start..]
            .iter()
            .map(|ch| {
                if *ch == '\n' {
                    '↵'
                } else if *ch == '\t' {
                    ' '
                } else {
                    *ch
                }
            })
            .collect::<String>();
        (clip(&text, width), cursor)
    }
}
struct State {
    mode: String,
    activity: String,
    draft: Draft,
    saved_draft: Option<Draft>,
    answer: Option<oneshot::Sender<String>>,
    partial: String,
    usage: super::progress::ReportedUsage,
    generated_characters: usize,
    estimate_pending: bool,
    started: Instant,
    activity_started: Instant,
    bindings: crate::keybindings::Keybindings,
    key_pending: crate::keybindings::Pending,
    details: super::input::editor::DetailsPanel,
    detail_entries: Vec<(String, String, String)>,
    thinking: String,
    history: Vec<String>,
    history_index: usize,
    history_draft: String,
    search_query: Option<String>,
}
impl State {
    fn new(mode: String) -> Self {
        Self {
            mode,
            activity: "Waiting for response".into(),
            draft: Draft::default(),
            saved_draft: None,
            answer: None,
            partial: String::new(),
            usage: Default::default(),
            generated_characters: 0,
            estimate_pending: false,
            started: Instant::now(),
            activity_started: Instant::now(),
            bindings: Default::default(),
            key_pending: Default::default(),
            details: Default::default(),
            detail_entries: Vec::new(),
            thinking: String::new(),
            history: Vec::new(),
            history_index: 0,
            history_draft: String::new(),
            search_query: None,
        }
    }
    fn detail(&mut self, id: String, title: String, body: String) {
        if let Some(entry) = self.detail_entries.iter_mut().find(|entry| entry.0 == id) {
            *entry = (id, title, body);
        } else {
            self.detail_entries.push((id, title, body));
        }
        self.details.update_entries(
            self.detail_entries
                .iter()
                .map(|(_, title, body)| (title.clone(), body.clone()))
                .collect(),
        );
    }
    fn question(&mut self, reply: oneshot::Sender<String>) {
        self.saved_draft = Some(std::mem::take(&mut self.draft));
        self.answer = Some(reply);
    }
    fn submit(&mut self, actions: &async_mpsc::UnboundedSender<Action>) -> Option<String> {
        let text = self.draft.take();
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(text);
            self.draft = self.saved_draft.take().unwrap_or_default();
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.history_index = self.history.len();
        self.history_draft = String::new();
        if let Some(question) = text.trim().strip_prefix("/btw ") {
            if question.trim().is_empty() {
                return Some("Usage: /btw QUESTION".into());
            }
            let _ = actions.send(Action::SideQuestion(question.trim().into()));
            return Some("Side question submitted; main task continues.".into());
        }
        if text.trim() == "/background" {
            let _ = actions.send(Action::Detach);
            return Some("Detaching from the active worker; its request continues.".into());
        }
        if text.trim().starts_with('/') {
            return Some("While working, use /btw QUESTION or type a prompt to queue; other commands are available after completion.".into());
        }
        let _ = actions.send(Action::Submit(text.clone()));
        Some(format!("\n{text}\n\nPrompt queued for after this turn."))
    }
}
fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        let size = ch.width().unwrap_or(0);
        if used + size > width {
            break;
        }
        used += size;
        result.push(ch);
    }
    result
}
fn terminal_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect::<String>()
        .replace('\t', "    ")
        .replace('\n', "\r\n")
}
fn content_budget(height: u16) -> usize {
    (height as usize).saturating_sub(9).clamp(2, 8)
}
/// Return complete display rows with their original UTF-8 byte endpoints.
fn visual_lines(text: &str, width: usize) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let size = ch.width().unwrap_or(0);
        if cells + size > width.max(1) && !line.is_empty() {
            result.push((std::mem::take(&mut line), index));
            cells = 0;
        }
        line.push(ch);
        cells += size;
        end = index + ch.len_utf8();
    }
    result.push((line, end));
    result
}
fn flush_wrapped(state: &mut State, screen: &mut Frame) -> io::Result<()> {
    let (width, height) = terminal::size().unwrap_or((80, 24));
    // One content row is reserved for the disclosure once tool/thinking exists.
    let budget = content_budget(height)
        .saturating_sub(usize::from(!state.detail_entries.is_empty()))
        .max(1);
    let rows = visual_lines(&state.partial, (width as usize).saturating_sub(1));
    if rows.len() > budget {
        let count = rows.len() - budget;
        let end = rows[count - 1].1;
        screen.log(
            &rows[..count]
                .iter()
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        state.partial.drain(..end);
    }
    Ok(())
}
#[derive(Default)]
struct Frame {
    lines: Vec<String>,
    cursor_up: usize,
    cursor_column: usize,
    size: (u16, u16),
    panel_toggle: Option<super::input::editor::HitTarget>,
    panel_height: usize,
    panel_viewport: usize,
}
impl Frame {
    fn anchor(&mut self, out: &mut String) {
        if self.cursor_up > 0 {
            out.push_str(&format!("\x1b[{}B", self.cursor_up));
        }
        out.push('\r');
        self.cursor_up = 0;
    }
    fn clear(&mut self) -> io::Result<()> {
        if self.lines.is_empty() {
            return Ok(());
        }
        let mut out = String::from("\x1b[?25l");
        self.anchor(&mut out);
        out.push_str("\x1b[2K");
        let rows = self.lines.len().min(
            terminal::size()
                .map(|(_, height)| usize::from(height))
                .unwrap_or(self.lines.len())
                .max(1),
        );
        for _ in 1..rows {
            out.push_str("\x1b[1A\r\x1b[2K");
        }
        out.push_str("\x1b[?25h");
        self.lines.clear();
        write(&out)
    }
    fn log(&mut self, text: &str) -> io::Result<()> {
        self.clear()?;
        write(&format!("{}\r\n", terminal_text(text)))
    }
    fn draw(&mut self, state: &mut State) -> io::Result<()> {
        let size = terminal::size().unwrap_or((80, 24));
        let width = (size.0 as usize).saturating_sub(1).max(4);
        let (draft, cursor) = state.draft.visible(width.saturating_sub(3));
        let usage = state
            .usage
            .summary()
            .map(|summary| format!(" \u{b7} {summary}"))
            .unwrap_or_default();
        let usage = if state.estimate_pending && state.generated_characters > 0 {
            format!(
                " \u{b7} ~{} generated tokens (estimate)",
                state.generated_characters.div_ceil(4)
            )
        } else {
            usage
        };
        let status = clip(
            &format!(
                "* {} ({}s){usage}",
                state.activity,
                if state.activity.contains("thinking") {
                    state.activity_started.elapsed().as_secs()
                } else {
                    state.started.elapsed().as_secs()
                }
            ),
            width,
        );
        let mode = if state.answer.is_some() {
            "Answer requested · Enter submits · Ctrl+C interrupts".to_owned()
        } else {
            format!(
                "{} · Enter queues · /btw question /background detach · Esc interrupts",
                state.mode
            )
        };
        let rule = "─".repeat(width);
        let budget = content_budget(size.1);
        let panel = state.details.render(width, budget);
        self.panel_toggle = panel.toggle;
        self.panel_height = panel.lines.len();
        self.panel_viewport = panel.viewport;
        let mut content = if state.details.expanded {
            panel
                .lines
                .iter()
                .map(|(text, _)| text.clone())
                .collect::<Vec<_>>()
        } else {
            let content = visual_lines(&state.partial, width)
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>();
            if !panel.lines.is_empty() {
                // Keep the disclosure reachable without hiding the streaming text.
                self.panel_toggle = None;
            }
            content
        };
        content.resize(budget, String::new());
        content.truncate(budget);
        let mut lines = vec![
            String::new(),
            format!(
                "\x1b[{}m{}\x1b[0m",
                crate::terminal::style::activity(),
                status
            ),
            clip(
                "  Tip: /btw asks a side question while the main task continues.",
                width,
            ),
            rule.clone(),
            format!("{} {draft}", if state.answer.is_some() { "?" } else { "›" }),
            rule,
            clip(&mode, width),
        ];
        if std::env::var_os("NO_COLOR").is_some() {
            lines[1] = status;
        } else {
            use crossterm::style::{ResetColor, SetForegroundColor};
            for index in [2, 6] {
                lines[index] = format!(
                    "{}{}{}",
                    SetForegroundColor(crate::terminal::style::muted()),
                    lines[index],
                    ResetColor
                );
            }
            for index in [3, 5] {
                lines[index] = format!(
                    "{}{}{}",
                    SetForegroundColor(crate::terminal::style::border()),
                    lines[index],
                    ResetColor
                );
            }
        }
        // The content viewport has a fixed height throughout the turn, so a long
        // stream grows into scrollback without making the composer jump.
        if !state.details.expanded && !panel.lines.is_empty() {
            let label = panel.lines[0].0.clone();
            content[budget - 1] = label;
            self.panel_height = budget;
            self.panel_toggle = panel.toggle.map(|mut hit| {
                hit.row = budget - 1;
                hit
            });
        }
        content.extend(lines);
        let mut lines = content;
        let rows = (size.1 as usize).saturating_sub(1).max(3);
        if lines.len() > rows {
            let removed = lines.len() - rows;
            lines.drain(0..removed);
            self.panel_toggle = self.panel_toggle.and_then(|mut hit| {
                hit.row = hit.row.checked_sub(removed)?;
                Some(hit)
            });
            self.panel_height = self.panel_height.saturating_sub(removed);
        }
        let input_row = lines.len().saturating_sub(3);
        let cursor_column = (cursor + 2).min(width);
        if self.lines == lines && self.size == size && self.cursor_column == cursor_column {
            return Ok(());
        }
        if !self.lines.is_empty() && (self.size != size || self.lines.len() != lines.len()) {
            self.clear()?;
        }
        let mut out = String::from("\x1b[?25l");
        self.anchor(&mut out);
        if self.lines.is_empty() {
            out.push_str(&lines.join("\r\n"));
        } else {
            if lines.len() != self.lines.len() {
                self.clear()?;
                out.push_str(&lines.join("\r\n"));
            } else {
                out.push_str(&row_updates(&self.lines, &lines));
            }
        }
        self.cursor_up = lines.len() - 1 - input_row;
        if self.cursor_up > 0 {
            out.push_str(&format!("\x1b[{}A", self.cursor_up));
        }
        out.push_str(&format!("\r\x1b[{}C\x1b[?25h", (cursor + 2).min(width)));
        self.lines = lines;
        self.size = size;
        self.cursor_column = cursor_column;
        write(&out)
    }
}
fn row_updates(previous: &[String], next: &[String]) -> String {
    let mut output = String::new();
    for (row, line) in next.iter().enumerate() {
        if previous.get(row) == Some(line) {
            continue;
        }
        let up = next.len() - 1 - row;
        if up > 0 {
            output.push_str(&format!("\x1b[{up}A"));
        }
        output.push('\r');
        output.push_str(line);
        output.push_str("\x1b[K");
        if up > 0 {
            output.push_str(&format!("\x1b[{up}B"));
        }
        output.push('\r');
    }
    output
}

fn write(text: &str) -> io::Result<()> {
    let mut output = io::stderr().lock();
    output.write_all(text.as_bytes())?;
    output.flush()
}
fn run(
    state: &mut State,
    screen: &mut Frame,
    updates: mpsc::Receiver<Update>,
    actions: async_mpsc::UnboundedSender<Action>,
) -> io::Result<()> {
    let mut last_tick = Instant::now();
    screen.draw(state)?;
    loop {
        let mut dirty = false;
        loop {
            match updates.try_recv() {
                Ok(Update::Text(text)) => {
                    state.generated_characters = state
                        .generated_characters
                        .saturating_add(text.chars().count());
                    state.estimate_pending = true;
                    if state.activity.contains("thinking")
                        || state.activity == "Waiting for response"
                    {
                        state.activity = "Composing".into();
                        state.activity_started = Instant::now();
                    }
                    state
                        .partial
                        .push_str(&terminal_text(&text).replace("\r\n", "\n"));
                    while let Some(index) = state.partial.find('\n') {
                        let line = state.partial[..index].to_owned();
                        state.partial.drain(..index + 1);
                        screen.log(&line)?;
                    }
                    dirty = true;
                }
                Ok(Update::Notice(text)) => {
                    if !state.partial.is_empty() {
                        screen.log(&std::mem::take(&mut state.partial))?;
                    }
                    screen.log(&text)?;
                    dirty = true;
                }
                Ok(Update::Styled(lines)) => {
                    if !state.partial.is_empty() {
                        screen.log(&std::mem::take(&mut state.partial))?;
                    }
                    screen.clear()?;
                    let width = terminal::size().unwrap_or((80, 24)).0.saturating_sub(1) as usize;
                    let color = std::env::var_os("NO_COLOR").is_none();
                    for line in lines {
                        write(&format!("{}\r\n", change_view::render(&line, width, color)))?;
                    }
                    dirty = true;
                }
                Ok(Update::Activity(activity)) => {
                    if state.activity != activity {
                        state.activity_started = Instant::now();
                    }
                    state.activity = activity;
                    dirty = true;
                }
                Ok(Update::Usage(value)) => {
                    state.usage.update(&value);
                    if value["output_tokens"]
                        .as_u64()
                        .or_else(|| value["completion_tokens"].as_u64())
                        .is_some_and(|value| value > 0)
                    {
                        state.estimate_pending = false;
                    }
                    dirty = true;
                }
                Ok(Update::Detail(id, title, body)) => {
                    if state.detail_entries.is_empty() {
                        execute!(io::stderr(), EnableMouseCapture)?;
                    }
                    state.detail(id, title, body);
                    dirty = true;
                }
                Ok(Update::Thinking(text)) => {
                    state.generated_characters = state
                        .generated_characters
                        .saturating_add(text.chars().count());
                    state.estimate_pending = true;
                    let remaining = 65536usize.saturating_sub(state.thinking.chars().count());
                    state.thinking.extend(text.chars().take(remaining));
                    if state.detail_entries.is_empty() {
                        execute!(io::stderr(), EnableMouseCapture)?;
                    }
                    state.detail("thinking".into(), "Thinking".into(), state.thinking.clone());
                    dirty = true;
                }
                Ok(Update::Question(text, reply)) => {
                    screen.log(&text)?;
                    state.question(reply);
                    dirty = true;
                }
                Ok(Update::Stop(error)) => {
                    if !state.partial.is_empty() {
                        screen.log(&std::mem::take(&mut state.partial))?;
                    }
                    let draft = if state.answer.is_some() {
                        state.saved_draft.take().unwrap_or_default().take()
                    } else {
                        state.draft.take()
                    };
                    if !draft.is_empty() {
                        let _ = actions.send(Action::Draft(draft));
                    }
                    screen.clear()?;
                    let footer = format!(
                        "Worked for {}s \u{b7} {} \u{b7} {}",
                        state.started.elapsed().as_secs(),
                        error.as_deref().unwrap_or("done"),
                        chrono::Local::now().format("%-I:%M %p")
                    );
                    if std::env::var_os("NO_COLOR").is_some() {
                        write(&format!("{}\r\n", terminal_text(&footer)))?;
                    } else {
                        write(&format!(
                            "{}\r\n",
                            change_view::render(&change_view::muted(&footer), usize::MAX, true)
                        ))?;
                    }
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if event::poll(Duration::from_millis(25))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = match state.bindings.resolve(key, &mut state.key_pending) {
                        crate::keybindings::Resolution::Key(key) => key,
                        crate::keybindings::Resolution::Consumed => continue,
                    };
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !(control && key.code == KeyCode::Char('r')) {
                        state.search_query = None;
                    }
                    if control && key.code == KeyCode::Char('b') {
                        let _ = actions.send(Action::Detach);
                    } else if control && key.code == KeyCode::Char('o') {
                        state.details.toggle();
                    } else if control && key.code == KeyCode::Char('r') && state.answer.is_none() {
                        let query = state
                            .search_query
                            .get_or_insert_with(|| state.draft.characters.iter().collect())
                            .clone();
                        if let Some(index) = (0..state.history_index)
                            .rev()
                            .find(|&index| state.history[index].contains(&query))
                        {
                            if state.history_index == state.history.len() {
                                state.history_draft = state.draft.characters.iter().collect();
                            }
                            state.history_index = index;
                            state.draft.set(&state.history[index]);
                        }
                    } else if control && key.code == KeyCode::Char('l') {
                        screen.clear()?;
                    } else if control && key.code == KeyCode::Char('u') {
                        state.draft.characters.drain(..state.draft.cursor);
                        state.draft.cursor = 0;
                    } else if control && key.code == KeyCode::Char('k') {
                        state.draft.characters.truncate(state.draft.cursor);
                    } else if control
                        && key.code == KeyCode::Char('d')
                        && state.draft.characters.is_empty()
                    {
                        let _ = actions.send(Action::Submit("/exit".into()));
                        let _ = actions.send(Action::Cancel);
                    } else if state.details.expanded
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                    {
                        state.details.scroll_by(
                            if key.code == KeyCode::PageUp {
                                -(screen.panel_viewport as isize)
                            } else {
                                screen.panel_viewport as isize
                            },
                            screen.panel_viewport,
                        );
                    } else if key.code == KeyCode::Up
                        && state.answer.is_none()
                        && state.history_index > 0
                    {
                        if state.history_index == state.history.len() {
                            state.history_draft = state.draft.characters.iter().collect();
                        }
                        state.history_index -= 1;
                        state.draft.set(&state.history[state.history_index]);
                    } else if key.code == KeyCode::Down
                        && state.answer.is_none()
                        && state.history_index < state.history.len()
                    {
                        state.history_index += 1;
                        let text = state
                            .history
                            .get(state.history_index)
                            .unwrap_or(&state.history_draft);
                        state.draft.set(text);
                    } else if (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || key.code == KeyCode::Esc
                    {
                        let _ = actions.send(Action::Cancel);
                        state.activity = "Cancelling".into();
                    } else {
                        match key.code {
                            KeyCode::Enter
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    || key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                state.draft.insert("\n")
                            }
                            KeyCode::Enter => {
                                if let Some(message) = state.submit(&actions) {
                                    screen.log(&message)?;
                                }
                            }
                            KeyCode::Backspace if state.draft.cursor > 0 => {
                                state.draft.cursor -= 1;
                                state.draft.characters.remove(state.draft.cursor);
                            }
                            KeyCode::Delete
                                if state.draft.cursor < state.draft.characters.len() =>
                            {
                                state.draft.characters.remove(state.draft.cursor);
                            }
                            KeyCode::Left => {
                                state.draft.cursor = state.draft.cursor.saturating_sub(1)
                            }
                            KeyCode::Right => {
                                state.draft.cursor =
                                    (state.draft.cursor + 1).min(state.draft.characters.len())
                            }
                            KeyCode::Home => state.draft.cursor = 0,
                            KeyCode::End => state.draft.cursor = state.draft.characters.len(),
                            KeyCode::Char(ch)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                state.draft.insert(&ch.to_string())
                            }
                            _ => {}
                        }
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    let (_, cursor_y) = crossterm::cursor::position()?;
                    let input_row = screen.lines.len().saturating_sub(1 + screen.cursor_up);
                    let origin = cursor_y.saturating_sub(input_row as u16);
                    if let Some(row) = mouse.row.checked_sub(origin) {
                        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                            && screen.panel_toggle.is_some_and(|hit| {
                                hit.contains(row as usize, mouse.column as usize)
                            })
                        {
                            state.details.toggle();
                            dirty = true;
                        } else if (row as usize) < screen.panel_height {
                            match mouse.kind {
                                MouseEventKind::ScrollDown => {
                                    state.details.scroll_by(3, screen.panel_viewport);
                                    dirty = true;
                                }
                                MouseEventKind::ScrollUp => {
                                    state.details.scroll_by(-3, screen.panel_viewport);
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    state.draft.insert(&text);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            dirty = true;
        }
        if dirty {
            flush_wrapped(state, screen)?;
            screen.draw(state)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrapped_rows_preserve_unicode_bytes_and_combining_marks() {
        let text = "ab\u{4e2d}e\u{301}xyz";
        let rows = visual_lines(text, 4);
        assert_eq!(
            rows.iter().map(|(row, _)| row.as_str()).collect::<String>(),
            text
        );
        assert_eq!(rows[0], ("ab\u{4e2d}".into(), 5));
        for (_, end) in rows {
            assert!(text.is_char_boundary(end));
        }
    }
    #[test]
    fn detail_updates_preserve_expansion_and_input_draft() {
        let mut state = State::new("default".into());
        state.draft.insert("unfinished prompt");
        state.detail("tool".into(), "Read".into(), "first".into());
        state.details.toggle();
        state.detail("tool".into(), "Read completed".into(), "second".into());
        assert!(state.details.expanded);
        let panel = state.details.render(60, 8);
        let hit = panel.toggle.unwrap();
        assert!(hit.contains(hit.row, 0));
        assert!(panel.lines.iter().any(|(line, _)| line.contains("second")));
        state.details.toggle();
        assert_eq!(state.draft.take(), "unfinished prompt");
    }
    #[test]
    fn noninteractive_ignores_invalid_keybinding_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keybindings.json");
        std::fs::write(&path, "not valid JSON").unwrap();
        assert!(TurnDisplay::new(false, "default", &path, vec![]).is_ok());
        assert!(TurnDisplay::new(true, "default", &path, vec![]).is_err());
    }

    #[test]
    fn stable_timer_updates_never_clear_the_frame_or_scroll() {
        let before = vec!["answer".into(), "working 1s".into(), "draft".into()];
        assert_eq!(row_updates(&before, &before), "");
        let after = vec!["answer".into(), "working 2s".into(), "draft".into()];
        let update = row_updates(&before, &after);
        assert!(update.contains("working 2s"));
        assert!(!update.contains("answer"));
        assert!(!update.contains("draft"));
        assert!(!update.contains('\n'));
        assert!(!update.contains("\x1b[2K"));
    }
    #[test]
    fn questions_preserve_drafts_and_do_not_queue_answers() {
        let mut state = State::new("ask".into());
        state.draft.insert("queued draft");
        let (reply, mut received) = oneshot::channel();
        state.question(reply);
        state.draft.insert("yes");
        let (actions, mut events) = async_mpsc::unbounded_channel();
        state.submit(&actions);
        assert_eq!(received.try_recv().unwrap(), "yes");
        assert_eq!(state.draft.take(), "queued draft");
        assert!(events.try_recv().is_err());
    }
    #[test]
    fn side_questions_and_queued_prompts_are_distinct() {
        let mut state = State::new("ask".into());
        let (send, mut receive) = async_mpsc::unbounded_channel();
        state.draft.insert("/btw question");
        state.submit(&send);
        assert!(matches!(
            receive.try_recv().unwrap(),
            Action::SideQuestion(_)
        ));
        state.draft.insert("next task");
        state.submit(&send);
        assert!(matches!(receive.try_recv().unwrap(), Action::Submit(_)));
        state.draft.insert("/permissions bypass");
        state.submit(&send);
        assert!(receive.try_recv().is_err());
    }
}

/// Unified API: exactly one of the live actor and legacy progress writes output.
pub struct TurnDisplay {
    pub live: Option<LiveTurn>,
    fallback: super::progress::TurnProgress,
    tools: std::collections::HashMap<String, (Instant, String)>,
    finished: std::collections::HashSet<String>,
}
impl TurnDisplay {
    pub fn new(
        interactive: bool,
        mode: &str,
        keybindings: &std::path::Path,
        history: Vec<String>,
    ) -> anyhow::Result<Self> {
        let live = if interactive {
            LiveTurn::start(mode, crate::keybindings::load(keybindings)?, history)
        } else {
            None
        };
        let mut fallback = super::progress::TurnProgress::new(interactive && live.is_none());
        fallback.set_permission_mode(mode);
        Ok(Self {
            live,
            fallback,
            tools: Default::default(),
            finished: Default::default(),
        })
    }
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }
    pub fn set_activity(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.activity(text);
        } else {
            self.fallback.set_activity(text);
        }
    }
    pub fn record_thinking(&self, text: &str) {
        if let Some(live) = &self.live {
            live.thinking(text);
        }
    }
    pub fn thinking(&mut self) {
        self.set_activity("Sketching \u{b7} thinking");
    }
    pub fn reported_usage(&mut self, value: &Value) {
        if let Some(live) = &self.live {
            live.usage(value);
        } else {
            self.fallback.reported_usage(value);
        }
    }
    pub fn before_text(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.text(text);
        } else {
            self.fallback.before_text(text);
        }
    }
    pub fn tool_started(&mut self, id: &str, name: &str, input: &Value) {
        if let Some(live) = &self.live {
            if self.tools.contains_key(id) || self.finished.contains(id) {
                return;
            }
            self.tools.insert(
                id.to_owned(),
                (Instant::now(), format!("Input:\n{}", input)),
            );
            live.detail(id, name, &format!("Input:\n{}", input));
            live.notice(&format!(
                "\u{2022} {}",
                super::progress::tool_preview(name, input)
            ));
            live.activity(&format!("Working: {name}"));
        } else {
            self.fallback.tool_started(id, name, input);
        }
    }
    pub fn tool_finished(&mut self, id: &str, name: &str, output: &str, failed: bool) {
        if let Some(live) = &self.live {
            if !self.finished.insert(id.to_owned()) {
                return;
            }
            let previous = self.tools.remove(id);
            let elapsed = previous.as_ref().map(|(start, _)| start.elapsed());
            let body = format!(
                "{}\n\nOutput:\n{}",
                previous.map(|(_, input)| input).unwrap_or_default(),
                output
            );
            live.detail(
                id,
                &format!("{name} - {}", if failed { "error" } else { "completed" }),
                &body,
            );
            let change = matches!(name, "Write" | "Edit" | "MultiEdit")
                .then(|| change_view::change(output, failed))
                .flatten();
            if let Some(lines) = change {
                live.detail(
                    id,
                    &format!("{name} - completed"),
                    &format!("{}\n\n{}", change_view::plain(&lines), body),
                );
                let mut preview = lines.iter().take(14).cloned().collect::<Vec<_>>();
                if lines.len() > 14 {
                    preview.push(change_view::success("More changes in details (Ctrl+O)"));
                }
                live.styled(preview);
            } else {
                let summary = super::progress::tool_result_summary(name, output, failed, elapsed);
                if failed {
                    live.notice(&summary);
                } else {
                    live.styled(vec![change_view::success(summary.trim())]);
                }
            }
            live.activity("Working");
        } else {
            self.fallback.tool_finished(id, name, output, failed);
        }
    }
    pub fn agent_progress(&mut self, id: &str, kind: &str, status: &str, tool: Option<&str>) {
        if let Some(live) = &self.live {
            live.notice(&format!(
                "  Agent {kind} [{id}] \u{b7} {status} {}",
                tool.unwrap_or("")
            ));
        } else {
            self.fallback.agent_progress(id, kind, status, tool);
        }
    }
    pub fn tick(&mut self) {
        if self.live.is_none() {
            self.fallback.tick();
        }
    }
    pub fn clear(&mut self) {
        if self.live.is_none() {
            self.fallback.clear();
        }
    }
    pub fn notice(&mut self, text: &str) {
        if let Some(live) = &self.live {
            live.notice(text);
        } else {
            self.fallback.clear();
            eprintln!("{text}");
        }
    }
    pub async fn action(&mut self) -> Option<Action> {
        match &mut self.live {
            Some(live) => live.actions.recv().await,
            None => std::future::pending().await,
        }
    }
    pub fn finish(&mut self, error: Option<&str>) -> Vec<Action> {
        if let Some(live) = self.live.take() {
            live.finish(error)
        } else {
            self.fallback.finish(error);
            Vec::new()
        }
    }
}
