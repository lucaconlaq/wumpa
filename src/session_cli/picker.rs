//! Terminal-only agent selection and naming. No launch side effects occur here.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::{
    Result,
    output::clean,
    sessions::{MAX_NAME_CHARACTERS, Session, SessionName},
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Attach(usize),
    Delete(usize),
    DeleteFolder,
    Create(Option<SessionName>),
    Cancel,
}

struct Picker {
    selection: ListState,
    naming: bool,
    name: String,
    error: Option<&'static str>,
}

impl Picker {
    fn new(count: usize) -> Self {
        Self {
            selection: ListState::default().with_selected(Some(0)),
            naming: count == 0,
            name: String::new(),
            error: None,
        }
    }

    fn append(&mut self, text: &str) {
        if text
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            self.error = Some("Names must be a single line without control characters.");
        } else if self.name.chars().count() + text.chars().count() > MAX_NAME_CHARACTERS {
            self.error = Some("Names can contain at most 64 characters.");
        } else {
            self.name.push_str(text);
            self.error = None;
        }
    }

    fn key(&mut self, key: KeyEvent, count: usize) -> Option<Choice> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(Choice::Cancel);
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Choice::DeleteFolder);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if self.naming {
            match key.code {
                KeyCode::Enter => match optional_name(&self.name) {
                    Ok(name) => return Some(Choice::Create(name)),
                    Err(error) => self.error = Some(error),
                },
                KeyCode::Backspace => {
                    self.name.pop();
                    self.error = None;
                }
                KeyCode::Char(character) => self.append(&character.to_string()),
                _ => {}
            }
        } else {
            let selected = self.selection.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selection
                        .select(Some(if selected == 0 { count } else { selected - 1 }))
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selection.select(Some((selected + 1) % (count + 1)))
                }
                KeyCode::Home => self.selection.select(Some(0)),
                KeyCode::End => self.selection.select(Some(count)),
                KeyCode::Char('q') => return Some(Choice::Cancel),
                KeyCode::Char('n') => self.naming = true,
                KeyCode::Char('d') | KeyCode::Delete if selected < count => {
                    return Some(Choice::Delete(selected));
                }
                KeyCode::Char('D') => return Some(Choice::DeleteFolder),
                KeyCode::Enter if selected == count => self.naming = true,
                KeyCode::Enter => return Some(Choice::Attach(selected)),
                _ => {}
            }
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, sessions: &[Session], checkout: &std::path::Path) {
        let area = frame.area();
        let accent = Style::default().fg(Color::Rgb(125, 211, 252));
        let muted = Style::default().fg(Color::Rgb(139, 153, 174));
        if area.width < 30 || area.height < 7 {
            frame.render_widget(
                Paragraph::new("  WUMPA\n  Resize to 30 × 7. Esc cancels."),
                area,
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("  WUMPA", accent.add_modifier(Modifier::BOLD)),
                    Span::styled(
                        if self.naming {
                            "  /  new agent"
                        } else {
                            "  /  agents"
                        },
                        muted,
                    ),
                ]),
                Line::styled(
                    format!("  {}", clean(&checkout.display().to_string())),
                    muted,
                ),
            ]),
            rows[0],
        );
        if self.naming {
            let form = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(rows[1]);
            frame.render_widget(Paragraph::new("  Session name"), form[0]);
            let input = Line::styled(format!("  {}▏", clean(&self.name)), accent);
            // Reserve a cell for a wide glyph crossing the left scroll boundary.
            let scroll = u16::try_from(
                input
                    .width()
                    .saturating_sub(form[1].width.saturating_sub(1) as usize),
            )
            .unwrap_or(u16::MAX);
            frame.render_widget(Paragraph::new(input).scroll((0, scroll)), form[1]);
            frame.render_widget(
                Paragraph::new("  Blank = default · max 64 · visible in dashboards").style(muted),
                form[2],
            );
        } else {
            let mut items = sessions
                .iter()
                .map(|session| {
                    let id: String = session.id.clone().into();
                    ListItem::new(Line::from(vec![
                        Span::raw(clean(&session.label)),
                        Span::styled(format!("  {} · {:?}", &id[..8], session.state), muted),
                    ]))
                })
                .collect::<Vec<_>>();
            items.push(ListItem::new("＋ New agent").style(accent));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(accent.add_modifier(Modifier::BOLD))
                    .highlight_symbol("  › "),
                rows[1],
                &mut self.selection,
            );
        }
        let help = if self.naming {
            "  enter create · ctrl-d delete folder · esc cancel"
        } else {
            "  enter open · d delete agent · D delete folder · n new · esc cancel"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    self.error
                        .map(|error| format!("  {error}"))
                        .unwrap_or_default(),
                    Style::default().fg(Color::Rgb(255, 132, 144)),
                ),
                Line::styled(help, muted),
            ]),
            rows[2],
        );
    }
}

#[cfg(test)]
mod deletion_tests {
    use super::*;

    #[test]
    fn deletion_choices_do_not_attach_or_create() {
        let mut picker = Picker::new(1);
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 1),
            Some(Choice::Delete(0))
        );
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE), 1),
            Some(Choice::DeleteFolder)
        );
        let mut picker = Picker::new(0);
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 0),
            Some(Choice::DeleteFolder)
        );
        assert_eq!(
            choose_plain(&[], &mut &b":delete\n"[..], &mut Vec::new()).unwrap(),
            Choice::DeleteFolder
        );
    }
}

fn optional_name(input: &str) -> std::result::Result<Option<SessionName>, &'static str> {
    if input.trim().is_empty()
        && !input
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        Ok(None)
    } else {
        SessionName::try_from(input.to_owned()).map(Some)
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_: libc::c_int) {
    // Only flag interruption here; terminal I/O is not signal-safe.
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Temporarily route catchable shutdown signals through terminal cleanup.
/// This CLI has one picker; restore inherited handlers before launch/attachment.
struct PickerSignals(Vec<(libc::c_int, libc::sigaction)>);

impl PickerSignals {
    fn install() -> io::Result<Self> {
        INTERRUPTED.store(false, Ordering::Relaxed);
        let mut owned = Self(Vec::new());
        // SAFETY: sigaction is a C output structure with a valid zeroed layout.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = interrupt as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: sa_mask is writable storage for a signal set.
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: previous is valid output storage. action contains a handler
            // that only stores a lock-free atomic flag; no I/O runs in the handler.
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            owned.0.push((signal, previous));
        }
        Ok(owned)
    }
}

impl Drop for PickerSignals {
    fn drop(&mut self) {
        for (signal, previous) in self.0.iter().rev() {
            // SAFETY: these are the actions saved for each installed signal.
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

/// Own only an inline viewport. Never enter or leave an alternate screen.
pub(super) struct InlineTerminal {
    terminal: Option<DefaultTerminal>,
    _signals: PickerSignals,
}

fn check_interruption() -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "selection interrupted; no agent was launched",
        )
        .into());
    }
    Ok(())
}

impl InlineTerminal {
    pub(super) fn new(height: u16) -> Result<Self> {
        // Install before raw mode; restore handlers only after terminal cleanup.
        let mut owned = Self {
            terminal: None,
            _signals: PickerSignals::install()?,
        };
        crossterm::terminal::enable_raw_mode()?;
        owned.terminal = Some(Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?);
        crossterm::execute!(io::stdout(), event::EnableBracketedPaste)?;
        Ok(owned)
    }

    pub(super) fn draw(&mut self, draw: impl FnOnce(&mut Frame)) -> Result<()> {
        check_interruption()?;
        self.terminal
            .as_mut()
            .ok_or("inline terminal initialization failed")?
            .draw(draw)?;
        Ok(())
    }

    pub(super) fn next_event(&mut self) -> Result<Option<Event>> {
        check_interruption()?;
        // Polling is bounded so shutdown never requires another keypress.
        let ready = match event::poll(Duration::from_millis(100)) {
            Ok(ready) => ready,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => false,
            Err(error) => return Err(error.into()),
        };
        check_interruption()?;
        if ready {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }

    pub(super) fn finish(self) -> Result<()> {
        drop(self);
        check_interruption()
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        if let Some(terminal) = self.terminal.as_mut() {
            // Erase only our transient picker, keeping prior shell output intact.
            let _ = terminal.clear();
            let _ = terminal.show_cursor();
        }
        let _ = crossterm::execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            crossterm::style::ResetColor
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub(super) fn choose(
    sessions: &[Session],
    checkout: &std::path::Path,
    plain: bool,
) -> Result<Choice> {
    if plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return choose_plain(sessions, &mut io::stdin().lock(), &mut io::stdout().lock());
    }
    let height = (sessions.len() + 1).clamp(3, 6) as u16 + 4;
    let mut terminal = InlineTerminal::new(height)?;
    let mut picker = Picker::new(sessions.len());
    let choice = loop {
        terminal.draw(|frame| picker.draw(frame, sessions, checkout))?;
        match terminal.next_event()? {
            Some(Event::Key(key)) => {
                if let Some(choice) = picker.key(key, sessions.len()) {
                    break choice;
                }
            }
            Some(Event::Paste(text)) if picker.naming => picker.append(&text),
            _ => {}
        }
    };
    terminal.finish()?;
    Ok(choice)
}

fn choose_plain(
    sessions: &[Session],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Choice> {
    if !sessions.is_empty() {
        for (index, session) in sessions.iter().enumerate() {
            let id: String = session.id.clone().into();
            writeln!(
                output,
                "{}) {} · {} · {:?}",
                index + 1,
                clean(&session.label),
                id,
                session.state
            )?;
        }
        write!(
            output,
            "n) Create new   d NUMBER) Delete agent   D) Delete folder   q) Cancel\nChoice: "
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(Choice::Cancel);
        }
        match line.trim() {
            "q" | "" => return Ok(Choice::Cancel),
            "n" => {}
            "D" => return Ok(Choice::DeleteFolder),
            deletion if deletion.starts_with("d ") => {
                let index = deletion[2..]
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|value| value.checked_sub(1))
                    .filter(|index| *index < sessions.len())
                    .ok_or("invalid agent selection")?;
                return Ok(Choice::Delete(index));
            }
            number => {
                let index = number
                    .parse::<usize>()
                    .ok()
                    .and_then(|value| value.checked_sub(1))
                    .filter(|index| *index < sessions.len())
                    .ok_or("invalid agent selection")?;
                return Ok(Choice::Attach(index));
            }
        }
    }
    loop {
        write!(
            output,
            "Session name (blank = default; :delete deletes folder): "
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(Choice::Cancel);
        }
        if line.trim() == ":delete" {
            return Ok(Choice::DeleteFolder);
        }
        // Remove only the line terminator, not embedded control characters.
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        match optional_name(line) {
            Ok(name) => return Ok(Choice::Create(name)),
            Err(error) => writeln!(output, "{error}")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        checkout::Observation,
        sessions::{CheckoutAssociation, SessionId, State},
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn sessions() -> Vec<Session> {
        let observed = Observation {
            path: "/checkout".into(),
            device: 1,
            inode: 2,
        };
        vec![Session {
            id: SessionId::try_from("a".repeat(32)).unwrap(),
            instance: "/private/control.sock".into(),
            checkout: CheckoutAssociation {
                root: observed.clone(),
                git_directory: observed.clone(),
                common_directory: observed,
            },
            label: "Review changes".into(),
            state: State::Running,
        }]
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn arrow_selection_create_naming_and_cancel() {
        let mut picker = Picker::new(2);
        picker.key(key(KeyCode::Down), 2);
        assert_eq!(picker.key(key(KeyCode::Enter), 2), Some(Choice::Attach(1)));
        picker.key(key(KeyCode::Down), 2);
        assert_eq!(picker.key(key(KeyCode::Enter), 2), None);
        assert!(picker.naming);
        picker.append("Fix auth 🤖");
        assert_eq!(
            picker.key(key(KeyCode::Enter), 2),
            Some(Choice::Create(Some(
                SessionName::try_from("Fix auth 🤖".to_owned()).unwrap()
            )))
        );
        assert_eq!(picker.key(key(KeyCode::Esc), 2), Some(Choice::Cancel));
        let mut empty = Picker::new(0);
        assert!(empty.naming);
        assert_eq!(
            empty.key(key(KeyCode::Enter), 0),
            Some(Choice::Create(None))
        );
        assert_eq!(
            empty.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 0),
            Some(Choice::Cancel)
        );
    }

    #[test]
    fn input_rejects_controls_oversize_paste_and_ignores_key_release() {
        let mut picker = Picker::new(0);
        picker.append("🙂");
        picker.key(key(KeyCode::Backspace), 0);
        assert!(picker.name.is_empty());
        for invalid in ["x".repeat(65), "bad\nname".into(), "\x1b[31m".into()] {
            picker.append(&invalid);
            assert!(picker.error.is_some());
            assert!(picker.name.is_empty());
        }
        picker.append("Valid name");
        assert!(picker.error.is_none());
        let mut released = key(KeyCode::Enter);
        released.kind = KeyEventKind::Release;
        assert_eq!(picker.key(released, 0), None);
    }

    #[test]
    fn plain_names_cancel_eof_defaults_and_resume() {
        let sessions = sessions();
        let mut output = Vec::new();
        assert_eq!(
            choose_plain(&sessions, &mut &b"1\n"[..], &mut output).unwrap(),
            Choice::Attach(0)
        );
        assert_eq!(
            choose_plain(&sessions, &mut &b"n\nNamed agent\n"[..], &mut output).unwrap(),
            Choice::Create(Some(
                SessionName::try_from("Named agent".to_owned()).unwrap()
            ))
        );
        assert_eq!(
            choose_plain(&[], &mut &b"\n"[..], &mut output).unwrap(),
            Choice::Create(None)
        );
        assert_eq!(
            choose_plain(&[], &mut &b""[..], &mut output).unwrap(),
            Choice::Cancel
        );
        assert_eq!(
            choose_plain(&sessions, &mut &b"q\n"[..], &mut output).unwrap(),
            Choice::Cancel
        );
        assert_eq!(
            choose_plain(&[], &mut &b"bad\tname\nGood\n"[..], &mut output).unwrap(),
            Choice::Create(Some(SessionName::try_from("Good".to_owned()).unwrap()))
        );
    }

    #[test]
    fn renders_picker_name_form_and_small_terminals() {
        let sessions = sessions();
        for (width, height) in [(90, 7), (40, 7), (20, 6)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut picker = Picker::new(sessions.len());
            terminal
                .draw(|frame| picker.draw(frame, &sessions, std::path::Path::new("/checkout")))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("WUMPA"));
            if width == 90 {
                assert!(text.contains("Review changes"));
                assert!(text.contains("New agent"));
                picker.key(key(KeyCode::Char('n')), sessions.len());
                picker.append("New task");
                terminal
                    .draw(|frame| picker.draw(frame, &sessions, std::path::Path::new("/checkout")))
                    .unwrap();
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("Session name"));
                assert!(text.contains("New task"));
                picker.name = "🤖".repeat(64);
                terminal
                    .draw(|frame| picker.draw(frame, &sessions, std::path::Path::new("/checkout")))
                    .unwrap();
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    text.contains('▏'),
                    "long names must keep the insertion point visible"
                );
                let many = vec![sessions[0].clone(); 50];
                let mut picker = Picker::new(many.len());
                picker.key(key(KeyCode::End), many.len());
                terminal
                    .draw(|frame| picker.draw(frame, &many, std::path::Path::new("/checkout")))
                    .unwrap();
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    text.contains("New agent"),
                    "selection must scroll within the compact viewport"
                );
            }
        }
    }
}
