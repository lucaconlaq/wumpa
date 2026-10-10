//! Dashboard layout, widgets, and styling; application transitions live in the parent.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{config::Connection, output::clean};

use super::{App, Form, FormKind, Pane};

const BG: Color = Color::Rgb(15, 18, 25);
pub(super) const PANEL: Color = Color::Rgb(22, 27, 37);
const TEXT: Color = Color::Rgb(230, 237, 247);
const MUTED: Color = Color::Rgb(139, 153, 174);
pub(super) const ACCENT: Color = Color::Rgb(125, 211, 252);
const STATUS: Color = Color::Rgb(134, 223, 182);
const RED: Color = Color::Rgb(255, 132, 144);

fn shortcut_line(actions: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in actions {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(ACCENT).bg(PANEL),
        ));
        spans.push(Span::styled(format!("{label} "), Style::default().fg(TEXT)));
    }
    Line::from(spans)
}

impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(BG).fg(TEXT)),
            area,
        );
        if area.width < 60 || area.height < 20 {
            frame.render_widget(
                Paragraph::new("WUMPA\nResize to at least 60 × 20.\nCtrl-C to quit.")
                    .style(Style::default().fg(ACCENT)),
                area,
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .margin(1)
        .split(area);
        let in_workspace = self.pane == Pane::Repositories;
        let server = self.workspace.map(|i| &self.config.servers[i]);
        let title = if in_workspace {
            server
                .map(|s| clean(&s.name))
                .unwrap_or_else(|| "Workspace".into())
        } else {
            String::new()
        };
        let subtitle = if in_workspace {
            server
                .map(|s| connection_label(&s.connection))
                .unwrap_or_default()
        } else {
            "Choose a server. Make yourself at home.".into()
        };
        let badge = if self.job.is_some() {
            if self.job.as_ref().is_some_and(|job| job.cloning) {
                "◌ Cloning"
            } else if self.connected.is_some() {
                "◌ Syncing"
            } else {
                "◌ Connecting"
            }
        } else if self.connected.is_some() {
            "● Connected"
        } else if in_workspace {
            "○ Offline"
        } else {
            ""
        };
        let header =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(15)]).split(rows[0]);
        let mut heading = vec![Span::styled(
            " WUMPA",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        if in_workspace {
            heading.push(Span::styled("  /  ", Style::default().fg(MUTED)));
            heading.push(Span::styled(
                title,
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(heading),
                Line::styled(format!(" {subtitle}"), Style::default().fg(MUTED)),
            ]),
            header[0],
        );
        frame.render_widget(
            Paragraph::new(badge)
                .right_aligned()
                .style(Style::default().fg(if self.connected.is_some() {
                    STATUS
                } else {
                    MUTED
                })),
            header[1],
        );
        if !in_workspace {
            let servers_block = workspace_panel(" Servers ");
            if self.config.servers.is_empty() {
                frame.render_widget(
                Paragraph::new(
                    "\n\nNo servers yet.\n\nYour workspace, wherever you code.\nPress n to add a local or SSH connection.",
                )
                .centered()
                .wrap(Wrap { trim: false })
                .block(servers_block)
                .style(Style::default().fg(MUTED)),
                rows[1],
            );
            } else {
                let items: Vec<_> = self
                    .config
                    .servers
                    .iter()
                    .enumerate()
                    .map(|(i, server)| {
                        let marker = if self.connected == Some(i) {
                            "●"
                        } else {
                            "○"
                        };
                        let target = connection_label(&server.connection);
                        ListItem::new(vec![
                            Line::from(format!("{marker} {}", clean(&server.name))),
                            Line::styled(format!("  {target}"), Style::default().fg(MUTED)),
                            Line::raw(""),
                        ])
                    })
                    .collect();
                frame.render_stateful_widget(
                    List::new(items)
                        .block(servers_block)
                        .highlight_style(selected())
                        .highlight_symbol("▌ "),
                    rows[1],
                    &mut self.servers,
                );
            }
        } else {
            let agent_warning = agent_discovery_warning(self.sessions.as_ref());
            let repos_title = format!(
                " Repositories  {}{} ",
                self.repositories.len(),
                agent_warning.unwrap_or("")
            );
            let repos_block = workspace_panel(&repos_title);
            if self.repositories.is_empty() {
                let message = if self.job.as_ref().is_some_and(|job| job.cloning) {
                    "\n\nCloning repository…\n\nApprove your SSH key if prompted.\nEsc cancels; s switches server."
                } else if self.job.is_some() {
                    "\n\nOpening your workspace…\n\nFetching repositories.\nPress s to choose another server."
                } else if self.connected.is_some() {
                    "\n\nA clean slate.\n\nPress a to clone your first GitHub repository.\n\nGit runs on this server using your SSH agent."
                } else {
                    "\n\nWorkspace unavailable.\n\nCheck that wumpa serve is running.\nPress r to retry, or s to switch server."
                };
                frame.render_widget(
                    Paragraph::new(message)
                        .centered()
                        .wrap(Wrap { trim: false })
                        .style(Style::default().fg(MUTED))
                        .block(repos_block),
                    rows[1],
                );
            } else {
                let checkout_rows = self.dashboard_rows();
                let display_ids = self
                    .sessions
                    .as_ref()
                    .map(crate::session_runtime::RemoteSnapshot::display_ids)
                    .unwrap_or_default();
                let items: Vec<_> = checkout_rows
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(row, (index, worktree, agent))| {
                        let repo = &self.repositories[index];
                        let mut changes_tree = worktree.filter(|_| agent.is_none());
                        let label =
                            if let Some(agent) = agent {
                                let next_sibling = checkout_rows.get(row + 1).is_some_and(
                                    |(next, tree, child)| {
                                        *next == index
                                            && if worktree.is_some() {
                                                child.is_some()
                                                    && tree.map(|tree| &tree.path)
                                                        == worktree.map(|tree| &tree.path)
                                            } else {
                                                true
                                            }
                                    },
                                );
                                let full_id: String = agent.id.clone().into();
                                let id = display_ids
                                    .get(&full_id)
                                    .map(String::as_str)
                                    .unwrap_or(&full_id);
                                format!(
                                    "{}{} 🤖 {} · {} · {}",
                                    if worktree.is_some() { "     " } else { "   " },
                                    if next_sibling { "├" } else { "└" },
                                    clean(agent.display_name()),
                                    id,
                                    agent_activity_label(agent, self.tick)
                                )
                            } else if let Some(worktree) = worktree {
                                let name = display_path(&worktree.path, self.home_dir.as_deref());
                                let connector = if checkout_rows.iter().skip(row + 1).any(
                                    |(next, child, agent)| {
                                        *next == index && child.is_some() && agent.is_none()
                                    },
                                ) {
                                    "├"
                                } else {
                                    "└"
                                };
                                format!(
                                    "   {connector} 🌲 {}{}",
                                    clean(&name),
                                    worktree_label(worktree)
                                )
                            } else {
                                let group =
                                    self.worktrees.iter().find(|group| group.url == repo.url);
                                let main = group.and_then(|group| {
                                    group.entries.iter().find(|entry| {
                                        Some(&entry.path) == repo.checkout_path.as_ref()
                                    })
                                });
                                changes_tree = main;
                                format!(
                                    " {}{}{}",
                                    repository_name(&repo.url),
                                    main.map(worktree_label).unwrap_or_default(),
                                    if group.is_some_and(|group| group.error.is_some()) {
                                        " [worktrees unavailable — i]"
                                    } else {
                                        ""
                                    }
                                )
                            };
                        let mut line = Line::styled(
                            label,
                            if agent.is_some() {
                                Style::default().fg(ACCENT)
                            } else if worktree.is_some() {
                                Style::default().fg(MUTED)
                            } else {
                                Style::default().add_modifier(Modifier::BOLD)
                            },
                        );
                        if let Some(tree) = changes_tree {
                            line.spans.extend(changes_label(tree).spans);
                        }
                        ListItem::new(line)
                    })
                    .collect();
                frame.render_stateful_widget(
                    List::new(items)
                        .block(repos_block)
                        .highlight_style(selected())
                        .highlight_symbol("▌"),
                    rows[1],
                    &mut self.repos,
                );
            }
        }
        let spinner = ["◐", "◓", "◑", "◒"][self.tick % 4];
        let prefix = if self.job.is_some() || self.zed_launch.is_some() {
            spinner
        } else if self.error {
            "!"
        } else {
            "·"
        };
        frame.render_widget(
            Paragraph::new(format!("{prefix} {}", clean(&self.status)))
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(if self.error { RED } else { MUTED }))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(PANEL)),
                ),
            rows[2],
        );
        let actions = if in_workspace && self.selected_agent().is_some() {
            vec![
                ("Enter", "Attach"),
                ("n", "New agent"),
                ("i", "Details"),
                ("z", "Zed"),
            ]
        } else if in_workspace {
            vec![
                ("n", "New agent"),
                ("a", "Clone"),
                ("i", "Details"),
                ("z", "Zed"),
                ("Enter", "SSH"),
            ]
        } else {
            vec![
                ("Enter", "Open"),
                ("i", "Details"),
                ("n", "New server"),
                ("d", "Remove"),
            ]
        };
        let navigation = if in_workspace {
            vec![
                ("c", "Clone saved"),
                ("d", "Delete"),
                ("r", "Refresh"),
                ("s", "Switch server"),
                ("q", "Quit"),
            ]
        } else if self.workspace.is_some() {
            vec![
                ("↑↓", "Navigate"),
                ("Esc", "Back to workspace"),
                ("q", "Quit"),
            ]
        } else {
            vec![("↑↓", "Navigate"), ("q", "Quit")]
        };
        frame.render_widget(
            Paragraph::new(vec![shortcut_line(&actions), shortcut_line(&navigation)]),
            rows[3],
        );
        if self.details {
            self.draw_details(frame);
        }
        if let Some(form) = &self.form {
            draw_form(frame, form, self.repository_dir.as_deref());
        }
        if let Some(prompt) = &self.delete_prompt {
            let width = 72.min(area.width.saturating_sub(2));
            let height = 12.min(area.height);
            let popup = Rect::new(
                (area.width - width) / 2,
                (area.height - height) / 2,
                width,
                height,
            );
            frame.render_widget(Clear, popup);
            let text = if self.delete_second {
                format!(
                    "\n{}\n\nType {} to confirm permanently deleting files:\n\n{}▏\n\nEnter confirm · Esc cancel",
                    clean(&prompt.description),
                    clean(&prompt.second),
                    clean(&self.delete_input)
                )
            } else {
                format!(
                    "\n{}\n\n[y] Delete permanently    [n / Esc] Cancel",
                    clean(&prompt.description)
                )
            };
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: true })
                    .block(panel(" Delete from server ", true)),
                popup,
            );
        }
        if let Some(index) = self.remove_target {
            let width = 64.min(area.width.saturating_sub(4));
            let popup = Rect::new((area.width - width) / 2, (area.height - 12) / 2, width, 12);
            frame.render_widget(Clear, popup);
            let text = format!(
                "\nRemove {:?}?\n\nOnly this saved connection will be removed.\nThe server keeps running; repositories stay intact.\n\n[y] Remove connection    [n / Esc] Cancel",
                clean(&self.config.servers[index].name)
            );
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: true })
                    .centered()
                    .block(panel(" Remove connection ", true)),
                popup,
            );
        }
    }
    fn draw_server_details(&mut self, frame: &mut Frame) {
        let Some(server) = self
            .servers
            .selected()
            .and_then(|i| self.config.servers.get(i))
        else {
            return;
        };
        let (status, version, error) = match &self.server_details {
            None => ("Contacting…", "Loading…", None),
            Some(Ok(response)) => (
                "Reachable",
                response
                    .server_version
                    .as_deref()
                    .unwrap_or("Unavailable — update the server"),
                response.error.as_deref(),
            ),
            Some(Err(error)) => ("Request failed", "Unavailable", Some(error.as_str())),
        };
        let mut text = format!(
            "Name: {}\n\nConnection: {}\n\nStatus: {}\n\nWumpa version: {}",
            clean(&server.name),
            connection_label(&server.connection),
            status,
            clean(version),
        );
        if let Some(error) = error {
            text.push_str(&format!("\n\nError: {}", clean(error)));
        }
        self.draw_details_popup(frame, " Server details ", &text, "↑↓ Scroll · i/Esc Close");
    }

    fn draw_details(&mut self, frame: &mut Frame) {
        if self.pane == Pane::Servers {
            self.draw_server_details(frame);
            return;
        }
        let Some((entry, worktree)) = self.selected_checkout() else {
            return;
        };
        let Some(server) = self
            .workspace
            .and_then(|index| self.config.servers.get(index))
        else {
            return;
        };
        let checkout = worktree
            .map(|worktree| &worktree.path)
            .or(entry.checkout_path.as_ref())
            .map(|path| display_path(path, self.home_dir.as_deref()))
            .unwrap_or_else(|| "None — clone this repository first".into());
        let root = self
            .repository_dir
            .as_ref()
            .map(|path| display_path(path, self.home_dir.as_deref()))
            .unwrap_or_else(|| "Unknown — update the server and refresh".into());
        let zed = self.zed_target().unwrap_or_else(|error| error.to_string());
        let mut text = format!(
            "Name: {}\n\nURL: {}\n\nState: {}\nCheckout: {}\n\nServer: {}\nConnection: {}\nServer root: {}\n\nZed: {}",
            repository_name(&entry.url),
            clean(&entry.url),
            if entry.checkout_path.is_some() {
                "Cloned"
            } else {
                "Saved — not cloned"
            },
            clean(&checkout),
            clean(&server.name),
            connection_label(&server.connection),
            clean(&root),
            clean(&zed)
        );
        if let Some(worktree) = worktree {
            text.push_str(&format!(
                "\n\nWorktree: {}{}",
                clean(&display_path(&worktree.path, self.home_dir.as_deref())),
                worktree_label(worktree)
            ));
        }
        if let Some(error) = self
            .worktrees
            .iter()
            .find(|group| group.url == entry.url)
            .and_then(|group| group.error.as_ref())
        {
            text.push_str(&format!("\n\nWorktree discovery: {}", clean(error)));
        }
        let footer = if self.selected_agent().is_some() {
            "↑↓ Scroll · i/Esc Close · z Open in Zed · Enter Attach"
        } else {
            "↑↓ Scroll · i/Esc Close · z Open in Zed · Enter SSH"
        };
        self.draw_details_popup(frame, " Repository details ", &text, footer);
    }

    fn draw_details_popup(&mut self, frame: &mut Frame, title: &str, text: &str, footer: &str) {
        let area = frame.area();
        let width = 88.min(area.width.saturating_sub(4));
        let height = 24.min(area.height.saturating_sub(4));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(panel(title, true), popup);
        let body = Rect::new(popup.x + 2, popup.y + 1, width - 4, height - 4);
        let lines = detail_lines(text, usize::from(body.width));
        let max_scroll =
            u16::try_from(lines.len().saturating_sub(usize::from(body.height))).unwrap_or(u16::MAX);
        self.details_scroll = self.details_scroll.min(max_scroll);
        frame.render_widget(Paragraph::new(lines).scroll((self.details_scroll, 0)), body);
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(ACCENT)),
            Rect::new(body.x, popup.y + height - 2, body.width, 1),
        );
    }
}

fn agent_activity_label(agent: &crate::session_runtime::Summary, tick: usize) -> String {
    if agent.state == crate::sessions::State::Running
        && agent.activity == crate::agent_activity::Activity::Working
    {
        let spinner = ["◐", "◓", "◑", "◒"][tick % 4];
        format!("{spinner} {}", agent.display_state())
    } else {
        agent.display_state().into()
    }
}

fn agent_discovery_warning(
    snapshot: Option<&crate::session_runtime::RemoteSnapshot>,
) -> Option<&'static str> {
    match snapshot {
        None => Some(" · Agents require daemon upgrade"),
        Some(snapshot) if snapshot.error.is_some() => Some(" · Agents unavailable"),
        Some(snapshot) if !snapshot.supported => Some(" · Agents unsupported"),
        Some(_) => None,
    }
}

#[test]
fn agent_discovery_states_are_distinct() {
    use crate::session_runtime::RemoteSnapshot;
    assert_eq!(
        agent_discovery_warning(None),
        Some(" · Agents require daemon upgrade")
    );
    let mut snapshot = RemoteSnapshot::default();
    assert_eq!(
        agent_discovery_warning(Some(&snapshot)),
        Some(" · Agents unsupported")
    );
    snapshot.supported = true;
    assert_eq!(agent_discovery_warning(Some(&snapshot)), None);
    snapshot.error = Some("discovery failed".into());
    assert_eq!(
        agent_discovery_warning(Some(&snapshot)),
        Some(" · Agents unavailable")
    );
}

// Abbreviate only a known server home, at a complete path-component boundary.
fn display_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(relative) = home
        .filter(|home| home.is_absolute() && home.parent().is_some())
        .and_then(|home| path.strip_prefix(home).ok())
    {
        if relative.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

#[test]
fn display_paths_use_only_the_known_server_home() {
    use std::path::Path;
    let home = Some(Path::new("/home/remote"));
    assert_eq!(display_path(Path::new("/home/remote"), home), "~");
    assert_eq!(
        display_path(Path::new("/home/remote/worktrees/app/balmy-gull/app"), home),
        "~/worktrees/app/balmy-gull/app"
    );
    for path in ["/home/remote-other/app", "/external/app"] {
        assert_eq!(display_path(Path::new(path), home), path);
    }
    assert_eq!(
        display_path(Path::new("/home/remote/app"), None),
        "/home/remote/app"
    );
    assert_eq!(
        display_path(Path::new("/app"), Some(Path::new("/"))),
        "/app"
    );
}

fn worktree_label(worktree: &crate::worktrees::Worktree) -> String {
    let branch = worktree.branch.as_deref().unwrap_or(if worktree.bare {
        "bare"
    } else if worktree.detached {
        "detached"
    } else {
        "unknown branch"
    });
    format!(
        " [{}]{}",
        clean(branch),
        if worktree.prunable { " [prunable]" } else { "" }
    )
}

fn changes_label(worktree: &crate::worktrees::Worktree) -> Line<'static> {
    if worktree.bare || worktree.prunable {
        return Line::default();
    }
    match &worktree.changes {
        Some(changes) if !changes.dirty => Line::default(),
        Some(changes) => Line::from(vec![
            Span::raw(" · "),
            Span::styled(format!("+{}", changes.added), Style::default().fg(STATUS)),
            Span::raw(" "),
            Span::styled(format!("-{}", changes.removed), Style::default().fg(RED)),
        ]),
        None => Line::raw(" · changes unavailable"),
    }
}

#[test]
fn checkout_change_labels_distinguish_clean_dirty_and_unknown() {
    use crate::worktrees::{Changes, Worktree};
    let mut tree = Worktree::default();
    assert_eq!(changes_label(&tree).to_string(), " · changes unavailable");
    tree.changes = Some(Changes::default());
    assert_eq!(changes_label(&tree).to_string(), "");
    tree.changes = Some(Changes {
        dirty: true,
        added: 12,
        removed: 3,
        untracked: 2,
    });
    let label = changes_label(&tree);
    assert_eq!(label.to_string(), " · +12 -3");
    assert_eq!(label.spans[1].style.fg, Some(STATUS));
    assert_eq!(label.spans[3].style.fg, Some(RED));
    tree.changes.as_mut().unwrap().untracked = 0;
    tree.changes.as_mut().unwrap().added = 0;
    tree.changes.as_mut().unwrap().removed = 0;
    assert_eq!(changes_label(&tree).to_string(), " · +0 -0");
    tree.bare = true;
    assert_eq!(changes_label(&tree).to_string(), "");
    tree.bare = false;
    tree.prunable = true;
    assert_eq!(changes_label(&tree).to_string(), "");
}

fn repository_name(url: &str) -> String {
    let url = clean(url);
    let name = url
        .trim()
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(&url);
    name.strip_suffix(".git").unwrap_or(name).to_owned()
}

// Hard-wrap details to measure the scroll range without enabling Ratatui's
// unstable rendered-line-info feature. Preserve spaces and Unicode boundaries.
fn detail_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut used = 0;
        for character in line.chars() {
            let size = Line::raw(character.to_string()).width();
            if used + size > width && !current.is_empty() {
                lines.push(Line::raw(std::mem::take(&mut current)));
                used = 0;
            }
            current.push(character);
            used += size;
        }
        lines.push(Line::raw(current));
    }
    lines
}

fn connection_label(connection: &Connection) -> String {
    match connection {
        Connection::Local { port } => format!("Local · 127.0.0.1:{port}"),
        Connection::Ssh { host, port } => format!("SSH · {} · :{port}", clean(host)),
    }
}

fn workspace_panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::TOP)
        .title(title)
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(MUTED))
        .style(Style::default().bg(BG).fg(TEXT))
        .padding(ratatui::widgets::Padding::new(1, 1, 1, 0))
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(PANEL).fg(TEXT))
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

fn selected() -> Style {
    Style::default().bg(Color::Rgb(32, 65, 89)).fg(ACCENT)
}

fn draw_server_form(frame: &mut Frame, form: &Form) {
    let area = frame.area();
    let width = 68.min(area.width.saturating_sub(4));
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - 18) / 2,
        width,
        18,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        popup,
    );
    let x = popup.x + 3;
    let y = popup.y + 1;
    let inner_width = popup.width - 6;
    frame.render_widget(
        Paragraph::new("New server").style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Rect::new(x, y, inner_width - 6, 1),
    );
    frame.render_widget(
        Paragraph::new(format!("{} / 3", form.field + 1))
            .right_aligned()
            .style(Style::default().fg(ACCENT)),
        Rect::new(x + inner_width - 6, y, 6, 1),
    );
    frame.render_widget(
        Paragraph::new("A familiar name. A workspace anywhere.").style(Style::default().fg(MUTED)),
        Rect::new(x, y + 1, inner_width, 1),
    );
    let fields = [
        ("Connection name", "e.g. Development"),
        ("SSH host · optional", "Leave blank for this machine"),
        ("Wumpa port", "7432"),
    ];
    for (i, (label, placeholder)) in fields.iter().enumerate() {
        let focused = i == form.field;
        let top = y + 3 + i as u16 * 3;
        frame.render_widget(
            Paragraph::new(*label).style(Style::default().fg(if focused { ACCENT } else { MUTED })),
            Rect::new(x, top, inner_width, 1),
        );
        let mut visible = form.values[i].as_str();
        while Line::raw(visible).width() > usize::from(inner_width.saturating_sub(4)) {
            visible = &visible[visible.chars().next().unwrap().len_utf8()..];
        }
        let content = if visible.is_empty() {
            Line::from(vec![
                Span::styled(
                    if focused { " ▏" } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(*placeholder, Style::default().fg(MUTED)),
            ])
        } else {
            Line::raw(format!(" {visible}{}", if focused { "▏" } else { "" }))
        };
        frame.render_widget(
            Paragraph::new(content).style(Style::default().fg(TEXT).bg(if focused {
                Color::Rgb(32, 48, 65)
            } else {
                BG
            })),
            Rect::new(x, top + 1, inner_width, 1),
        );
    }
    let local = form.values[1].trim().is_empty();
    frame.render_widget(
        Paragraph::new(if local {
            "○ Local connection · no SSH required"
        } else {
            "↗ SSH connection · uses your SSH config and keys"
        })
        .style(Style::default().fg(MUTED)),
        Rect::new(x, y + 11, inner_width, 1),
    );
    frame.render_widget(
        Paragraph::new(clean(&form.error))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(RED)),
        Rect::new(x, y + 12, inner_width, 2),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if form.field == 2 {
                    " Enter  Save server "
                } else {
                    " Enter  Continue "
                },
                Style::default()
                    .bg(ACCENT)
                    .fg(BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Tab Next   Esc Cancel", Style::default().fg(MUTED)),
        ])),
        Rect::new(x, y + 15, inner_width, 1),
    );
}

fn draw_form(frame: &mut Frame, form: &Form, root: Option<&std::path::Path>) {
    if form.kind == FormKind::Server {
        draw_server_form(frame, form);
        return;
    }
    let area = frame.area();
    let width = 64.min(area.width.saturating_sub(4));
    let height = 18;
    let popup = Rect::new(
        (area.width - width) / 2,
        (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(panel(" Clone repository ", true), popup);
    let inner = Rect::new(popup.x + 2, popup.y + 1, popup.width - 4, popup.height - 2);
    let help = "GitHub only · e.g. owner/repo";
    frame.render_widget(
        Paragraph::new(help)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let labels = ["GitHub repository · owner/repo", "Folder name · optional"];
    for (i, label) in labels.iter().enumerate() {
        let rect = Rect::new(inner.x, inner.y + 1 + i as u16 * 3, inner.width, 3);
        let focused = i == form.field;
        // Saved URLs come from the server; escape controls before rendering.
        // Inputs append at the end. Show the tail, retaining Unicode boundaries.
        let value = clean(&form.values[i]);
        let mut visible = value.as_str();
        while Line::raw(visible).width() > usize::from(rect.width.saturating_sub(4)) {
            visible = &visible[visible.chars().next().unwrap().len_utf8()..];
        }
        let text = format!("{visible}{}", if focused { "▏" } else { "" });
        frame.render_widget(Paragraph::new(text).block(panel(label, focused)), rect);
    }
    let folder = (!form.values[1].trim().is_empty()).then_some(form.values[1].trim());
    let destination = form
        .repository_url()
        .and_then(|url| crate::repository::folder_name(&url, folder))
        .ok()
        .and_then(|name| root.map(|root| root.join(name)));
    for (offset, label, path) in [
        (7, "Server root", root),
        (9, "Destination", destination.as_deref()),
    ] {
        let text = path
            .map(|path| clean(&path.display().to_string()))
            .unwrap_or_else(|| {
                if root.is_none() {
                    "Update the server and refresh".into()
                } else {
                    "Enter owner/repo and an optional folder".into()
                }
            });
        frame.render_widget(
            Paragraph::new(format!("{label}: {text}"))
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y + offset, inner.width, 2),
        );
    }
    let bottom = inner.y + inner.height - 4;
    frame.render_widget(
        Paragraph::new(clean(&form.error))
            .style(Style::default().fg(RED))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, bottom, inner.width, 3),
    );
    frame.render_widget(
        Paragraph::new("Tab next · Enter clone/next · Esc cancel · ^U clear")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, bottom + 3, inner.width, 1),
    );
}
