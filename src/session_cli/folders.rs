//! Friendly tracked-folder recovery, using only daemon-advertised local choices.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use super::picker::InlineTerminal;
use crate::{Result, checkout::TrackedFolder, output::clean};

struct FolderPicker {
    selected: ListState,
    error: Option<String>,
}

impl FolderPicker {
    fn new(count: usize) -> Self {
        Self {
            selected: ListState::default().with_selected((count > 0).then_some(0)),
            error: None,
        }
    }

    fn key(&mut self, key: KeyEvent, folders: &[TrackedFolder]) -> Option<Option<PathBuf>> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(None);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || folders.is_empty()
        {
            return None;
        }
        let selected = self.selected.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected.select(Some(if selected == 0 {
                folders.len() - 1
            } else {
                selected - 1
            })),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected.select(Some((selected + 1) % folders.len()))
            }
            KeyCode::Home => self.selected.select(Some(0)),
            KeyCode::End => self.selected.select(Some(folders.len() - 1)),
            KeyCode::Enter => {
                if let Some(error) = &folders[selected].error {
                    self.error = Some(error.clone());
                } else {
                    return Some(Some(folders[selected].path.clone()));
                }
            }
            _ => {}
        }
        if self.selected.selected() != Some(selected) {
            self.error = None;
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, folders: &[TrackedFolder], heading: &str) {
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
                    Span::styled("  /  tracked folders", muted),
                ]),
                Line::from(format!("  {heading}")),
            ]),
            rows[0],
        );
        if folders.is_empty() {
            frame.render_widget(Paragraph::new("  No tracked folders on this daemon.\n  Add a checkout in the Wumpa dashboard first."), rows[1]);
        } else {
            let items = folders
                .iter()
                .map(|folder| {
                    ListItem::new(Line::from(vec![
                        Span::raw(clean(&folder.path.display().to_string())),
                        Span::styled(
                            if folder.error.is_some() {
                                " · unavailable"
                            } else {
                                ""
                            },
                            muted,
                        ),
                    ]))
                })
                .collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(accent.add_modifier(Modifier::BOLD))
                    .highlight_symbol("  > "),
                rows[1],
                &mut self.selected,
            );
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    self.error
                        .as_deref()
                        .map(clean)
                        .unwrap_or_else(|| "  Opens an agent there; your shell stays here.".into()),
                    if self.error.is_some() {
                        Style::default().fg(Color::Rgb(255, 132, 144))
                    } else {
                        muted
                    },
                ),
                Line::styled("  ↑↓ select · enter open · esc cancel", muted),
            ]),
            rows[2],
        );
    }
}

fn heading(folders: &[TrackedFolder], directory: &Path) -> &'static str {
    let directory = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    // Prefixes only choose diagnostic wording; they never establish Git membership.
    if folders
        .iter()
        .any(|folder| folder.error.is_some() || directory.starts_with(&folder.path))
    {
        "We couldn't validate this folder."
    } else {
        "You're not inside a Wumpa-tracked folder."
    }
}

pub(super) fn choose(
    folders: &[TrackedFolder],
    directory: &Path,
    plain: bool,
) -> Result<Option<PathBuf>> {
    let heading = heading(folders, directory);
    let selected = if plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        choose_plain(
            folders,
            heading,
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        )?
    } else {
        let mut terminal = InlineTerminal::new(folders.len().clamp(3, 6) as u16 + 4)?;
        let mut picker = FolderPicker::new(folders.len());
        let selected = loop {
            terminal.draw(|frame| picker.draw(frame, folders, heading))?;
            if let Some(Event::Key(key)) = terminal.next_event()? {
                if let Some(selected) = picker.key(key, folders) {
                    break selected;
                }
            }
        };
        terminal.finish()?;
        selected
    };
    Ok(selected)
}

fn choose_plain(
    folders: &[TrackedFolder],
    heading: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<PathBuf>> {
    writeln!(output, "{heading}")?;
    if folders.is_empty() {
        writeln!(
            output,
            "No tracked folders on this daemon. Add a checkout in the Wumpa dashboard first."
        )?;
        return Ok(None);
    }
    for (index, folder) in folders.iter().enumerate() {
        writeln!(
            output,
            "{}) {}{}",
            index + 1,
            clean(&folder.path.display().to_string()),
            if folder.error.is_some() {
                " · unavailable"
            } else {
                ""
            }
        )?;
    }
    loop {
        write!(output, "Choose a folder (q cancels): ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 || matches!(line.trim(), "q" | "") {
            return Ok(None);
        }
        let index = line
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < folders.len());
        let Some(index) = index else {
            writeln!(output, "Choose a listed folder number.")?;
            continue;
        };
        if let Some(error) = &folders[index].error {
            writeln!(output, "{}", clean(error))?;
            continue;
        }
        return Ok(Some(folders[index].path.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn folders() -> Vec<TrackedFolder> {
        vec![
            TrackedFolder {
                path: "/tracked/checkout".into(),
                error: None,
            },
            TrackedFolder {
                path: "/tracked/missing".into(),
                error: Some("Folder unavailable.".into()),
            },
        ]
    }

    #[test]
    fn selection_cancel_and_unavailable_entries() {
        let folders = folders();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut picker = FolderPicker::new(folders.len());
        assert_eq!(
            picker.key(key(KeyCode::Enter), &folders),
            Some(Some(folders[0].path.clone()))
        );
        picker.key(key(KeyCode::Down), &folders);
        assert_eq!(picker.key(key(KeyCode::Enter), &folders), None);
        assert!(picker.error.is_some());
        picker.key(key(KeyCode::Up), &folders);
        assert!(picker.error.is_none());
        assert_eq!(picker.key(key(KeyCode::Esc), &folders), Some(None));
        assert_eq!(FolderPicker::new(0).key(key(KeyCode::Enter), &[]), None);
    }

    #[test]
    fn friendly_diagnostics_and_plain_selection() {
        let folders = folders();
        assert_eq!(
            heading(&folders[..1], Path::new("/untracked")),
            "You're not inside a Wumpa-tracked folder."
        );
        assert_eq!(
            heading(&folders, Path::new("/untracked")),
            "We couldn't validate this folder."
        );
        let mut output = Vec::new();
        assert_eq!(
            choose_plain(
                &folders,
                "Choose a tracked folder",
                &mut &b"2\n1\n"[..],
                &mut output
            )
            .unwrap(),
            Some(folders[0].path.clone())
        );
        assert_eq!(
            choose_plain(&folders, "Choose", &mut &b"q\n"[..], &mut output).unwrap(),
            None
        );
        assert_eq!(
            choose_plain(&folders, "Choose", &mut &b""[..], &mut output).unwrap(),
            None
        );
        assert!(!String::from_utf8_lossy(&output).contains("cd --"));
    }

    #[test]
    fn renders_in_the_same_compact_inline_style() {
        let folders = folders();
        for (width, height) in [(90, 8), (30, 7), (20, 6)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut picker = FolderPicker::new(folders.len());
            terminal
                .draw(|frame| {
                    picker.draw(frame, &folders, "You're not inside a Wumpa-tracked folder.")
                })
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
                assert!(text.contains("Wumpa-tracked folder"));
                let first_choice = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .skip(width as usize * 2)
                    .take(width as usize)
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert_eq!(first_choice.trim_end(), "  > /tracked/checkout");
                assert!(text.contains("/tracked/missing · unavailable"));
                assert!(!text.contains("Shell shortcut"));
                assert!(!text.contains("cd --"));
                assert!(!text.contains("To move your shell"));
            }
        }
    }
}
