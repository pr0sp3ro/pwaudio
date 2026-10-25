use std::{io, sync::atomic::Ordering, time::Duration};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    model::{Node, NodeRef, Profile, Row, common_volume, rows},
    runtime::{Action, Snapshot, Worker},
};

const SECTIONS: [&str; 3] = ["Output devices", "Input devices", "Applications"];

enum Picker {
    Target {
        streams: Vec<NodeRef>,
        options: Vec<Node>,
        selection: ListState,
    },
    Profile {
        options: Vec<Profile>,
        selection: ListState,
    },
}

#[derive(Default)]
struct Panel {
    section: usize,
    selections: [ListState; 3],
    selected_keys: [Option<String>; 3],
    detail: Option<Row>,
    detail_selection: ListState,
    detail_key: Option<NodeRef>,
    picker: Option<Picker>,
    notice: Option<String>,
}

pub fn run(worker: &Worker) -> Result<(), String> {
    if !io::IsTerminal::is_terminal(&io::stdin()) || !io::IsTerminal::is_terminal(&io::stdout()) {
        return Err("The audio panel needs an interactive terminal. Use pwaudio daemon for background enforcement".into());
    }

    let mut terminal = ratatui::init();
    let result = run_panel(worker, &mut terminal);
    ratatui::restore();
    result.map_err(|e| e.to_string())
}

fn run_panel(worker: &Worker, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    let mut panel = Panel::default();

    while !worker.stop.load(Ordering::Relaxed) {
        let snapshot = worker.snapshot();
        let sections = rows(&snapshot.graph.nodes(), &snapshot.config);
        panel.reconcile(&sections);
        terminal.draw(|frame| panel.draw(frame, &snapshot, &sections))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && panel.key(key, &snapshot, &sections, worker)
        {
            break;
        }
    }

    Ok(())
}

impl Panel {
    fn reconcile(&mut self, sections: &[Vec<Row>; 3]) {
        for (index, section) in sections.iter().enumerate() {
            let selection = self.selected_keys[index]
                .as_ref()
                .and_then(|key| section.iter().position(|row| &row.key == key))
                .or(self.selections[index].selected());
            select_valid(&mut self.selections[index], selection, section.len());
            self.selected_keys[index] = self.selections[index]
                .selected()
                .map(|i| section[i].key.clone());
        }

        if let Some(detail) = &mut self.detail {
            if let Some(current) = sections.iter().flatten().find(|row| {
                row.key == detail.key
                    || (detail.identity.is_some()
                        && row.identity == detail.identity
                        && row.capture == detail.capture)
            }) {
                *detail = current.clone();
            } else {
                // Never retain a vanished object or a reused numeric ID in details.
                let nodes: Vec<_> = sections
                    .iter()
                    .flatten()
                    .flat_map(|row| &row.nodes)
                    .collect();
                detail.nodes.retain(|node| {
                    nodes
                        .iter()
                        .any(|current| current.reference == node.reference)
                });
            }

            let selection = self
                .detail_key
                .as_ref()
                .and_then(|key| detail.nodes.iter().position(|node| &node.reference == key))
                .or(self.detail_selection.selected());
            select_valid(&mut self.detail_selection, selection, detail.nodes.len());
            self.detail_key = self
                .detail_selection
                .selected()
                .map(|i| detail.nodes[i].reference.clone());
        }
    }

    fn selected<'a>(&'a self, sections: &'a [Vec<Row>; 3]) -> Option<&'a Row> {
        self.detail.as_ref().or_else(|| {
            self.selections[self.section]
                .selected()
                .and_then(|i| sections[self.section].get(i))
        })
    }

    fn key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        sections: &[Vec<Row>; 3],
        worker: &Worker,
    ) -> bool {
        if key.code == KeyCode::Char('q')
            || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return true;
        }

        if self.picker.is_some() {
            self.picker_key(key, worker);
            return false;
        }

        self.notice = None;

        match key.code {
            KeyCode::Esc => {
                self.detail = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.section = (self.section + if backwards { 2 } else { 1 }) % 3;
                self.detail = None;
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                let down = matches!(key.code, KeyCode::Char('j') | KeyCode::Down);

                if let Some(detail) = &self.detail {
                    move_selection(&mut self.detail_selection, detail.nodes.len(), down);
                    self.detail_key = self
                        .detail_selection
                        .selected()
                        .map(|i| detail.nodes[i].reference.clone());
                } else {
                    move_selection(
                        &mut self.selections[self.section],
                        sections[self.section].len(),
                        down,
                    );
                    self.selected_keys[self.section] = self.selections[self.section]
                        .selected()
                        .map(|i| sections[self.section][i].key.clone());
                }
            }
            KeyCode::Char('r') => worker.send(Action::Refresh),
            KeyCode::Enter if self.detail.is_none() => {
                self.detail = self.selected(sections).cloned();
                self.detail_selection = ListState::default();
                self.detail_key = None;
            }
            code => {
                let Some(row) = self.selected(sections).cloned() else {
                    return false;
                };
                let nodes = if self.detail.is_some() && row.nodes.iter().any(Node::is_stream) {
                    self.detail_selection
                        .selected()
                        .and_then(|i| row.nodes.get(i))
                        .cloned()
                        .into_iter()
                        .collect()
                } else {
                    row.nodes.clone()
                };
                let refs = references(&nodes);

                match code {
                    KeyCode::Char('h') | KeyCode::Left => worker.send(Action::Volume(refs, -0.05)),
                    KeyCode::Char('l') | KeyCode::Right => worker.send(Action::Volume(refs, 0.05)),
                    KeyCode::Char('m') => worker.send(Action::Mute(refs)),
                    KeyCode::Char('M') => {
                        if row.capture || self.section != 2 {
                            self.notice = Some(
                                "Persistent mute is available for playback applications only"
                                    .into(),
                            );
                        } else if let Some(identity) = row.identity {
                            worker.send(Action::Persistent(references(&row.nodes), identity));
                        } else {
                            self.notice = Some(
                                "This application has no binary or application name to match"
                                    .into(),
                            );
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(node) = nodes.first() {
                            worker.send(Action::Default(node.reference.clone()));
                        }
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        let streams = if code == KeyCode::Char('T') {
                            &row.nodes
                        } else {
                            &nodes
                        };

                        if streams.is_empty() || streams.iter().any(|node| !node.is_stream()) {
                            self.notice = Some(
                                "Select an active application stream to choose its target".into(),
                            );
                        } else {
                            let capture = streams[0].is_capture();
                            let options: Vec<_> = snapshot
                                .graph
                                .nodes()
                                .into_iter()
                                .filter(|node| {
                                    if capture {
                                        node.is_source()
                                    } else {
                                        node.is_sink()
                                    }
                                })
                                .collect();

                            if options.is_empty() {
                                self.notice = Some("No compatible target devices".into());
                            } else {
                                self.picker = Some(Picker::Target {
                                    streams: references(streams),
                                    options,
                                    selection: ListState::default().with_selected(Some(0)),
                                });
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        let options = nodes
                            .first()
                            .map(|node| snapshot.graph.profiles(node))
                            .unwrap_or_default();

                        if options.is_empty() || nodes.first().is_some_and(Node::is_stream) {
                            self.notice =
                                Some("Select a device whose audio card exposes profiles".into());
                        } else {
                            let selected = options
                                .iter()
                                .position(|profile| profile.current)
                                .unwrap_or(0);
                            self.picker = Some(Picker::Profile {
                                options,
                                selection: ListState::default().with_selected(Some(selected)),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        false
    }

    fn picker_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                let down = matches!(key.code, KeyCode::Char('j') | KeyCode::Down);
                match picker {
                    Picker::Target {
                        options, selection, ..
                    } => move_selection(selection, options.len(), down),
                    Picker::Profile { options, selection } => {
                        move_selection(selection, options.len(), down)
                    }
                }
            }
            KeyCode::Enter => {
                match picker {
                    Picker::Target {
                        streams,
                        options,
                        selection,
                    } => {
                        if let Some(target) = selection.selected().and_then(|i| options.get(i)) {
                            worker.send(Action::Route(streams.clone(), target.reference.clone()));
                        }
                    }
                    Picker::Profile { options, selection } => {
                        if let Some(profile) = selection.selected().and_then(|i| options.get(i)) {
                            worker.send(Action::Profile(profile.clone()));
                        }
                    }
                }

                self.picker = None;
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, snapshot: &Snapshot, sections: &[Vec<Row>; 3]) {
        let area = frame.area();

        if area.width < 32 || area.height < 18 {
            frame.render_widget(
                Paragraph::new("Please resize the terminal to at least 32 x 18. Press q to quit.")
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }

        let connection = if snapshot.connected {
            "connected"
        } else {
            "disconnected"
        };
        let enforcement = if snapshot.daemon_running {
            "daemon active"
        } else {
            "mute enforcement: TUI only"
        };
        let border = Block::default()
            .title(format!(" Audio | {connection} | {enforcement} "))
            .borders(Borders::ALL);
        let inner = border.inner(area);
        frame.render_widget(border, area);
        let layout = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .split(inner);

        if let Some(detail) = &self.detail {
            draw_detail(
                frame,
                layout[0],
                detail,
                &mut self.detail_selection,
                snapshot,
            );
        } else {
            let areas = Layout::vertical([Constraint::Ratio(1, 3); 3]).split(layout[0]);

            for index in 0..3 {
                let block = Block::default()
                    .title(format!(" {} ", SECTIONS[index]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if self.section == index {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }));
                let width = areas[index].width.saturating_sub(4) as usize;
                let items: Vec<_> = if sections[index].is_empty() {
                    vec![ListItem::new("  No devices or applications")]
                } else {
                    sections[index]
                        .iter()
                        .map(|row| ListItem::new(row_line(row, snapshot, width)))
                        .collect()
                };
                let active = self.section == index;
                let list = List::new(items)
                    .block(block)
                    .highlight_symbol(if active { "> " } else { "  " })
                    .highlight_style(if active {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    });
                frame.render_stateful_widget(list, areas[index], &mut self.selections[index]);
            }
        }

        let message = self.notice.as_deref().unwrap_or(&snapshot.message);
        let status = vec![
            Line::from(clean(message)),
            Line::styled(
                clean(snapshot.config_error.as_deref().unwrap_or("")),
                Style::default().fg(Color::Red),
            ),
        ];
        frame.render_widget(Paragraph::new(status), layout[1]);
        let help = if self.detail.is_some() {
            "j/k select  h/l volume  m mute  M persistent\nt target  T all streams  p card profile  Esc back  q quit"
        } else {
            "j/k select  h/l volume  m mute  M persistent  d default\nTab section  Enter details  t target  p profile  r refresh  q quit"
        };
        frame.render_widget(
            Paragraph::new(help).style(Style::default().fg(Color::Gray)),
            layout[2],
        );

        if let Some(picker) = &mut self.picker {
            draw_picker(frame, area, picker);
        }
    }
}

fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    row: &Row,
    selection: &mut ListState,
    snapshot: &Snapshot,
) {
    let block = Block::default()
        .title(format!(" {} | details ", clean(&row.name)))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let layout = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    let description = if row.nodes.first().is_some_and(Node::is_stream) {
        "h/l and m affect the selected stream. t selects its target. T routes all streams."
    } else if row.nodes.is_empty() {
        "Inactive application. M toggles the saved persistent mute rule."
    } else {
        "p selects an audio card profile. Changing it affects the entire card."
    };
    frame.render_widget(
        Paragraph::new(description).wrap(Wrap { trim: true }),
        layout[0],
    );

    if row.nodes.first().is_some_and(Node::is_stream) {
        let items: Vec<_> = row
            .nodes
            .iter()
            .map(|node| {
                let stream = Row {
                    key: node.reference.serial.clone(),
                    name: node
                        .props
                        .get("media.name")
                        .cloned()
                        .unwrap_or_else(|| node.name.clone()),
                    nodes: vec![node.clone()],
                    identity: None,
                    persistent: snapshot.config.persistent(node),
                    capture: node.is_capture(),
                };
                let targets = snapshot.graph.targets(node);
                ListItem::new(vec![
                    row_line(&stream, snapshot, inner.width.saturating_sub(2) as usize),
                    Line::from(format!(
                        "  #{} -> {}",
                        node.reference.id,
                        if targets.is_empty() {
                            "unconnected".into()
                        } else {
                            clean(&targets.join(", "))
                        }
                    )),
                ])
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(Style::default().fg(Color::Cyan)),
            layout[1],
            selection,
        );
    } else {
        let mut lines = vec![row_line(row, snapshot, inner.width as usize)];

        if let Some(node) = row.nodes.first() {
            lines.push(Line::from(format!(
                "ID: {} | serial: {}",
                node.reference.id,
                clean(&node.reference.serial)
            )));

            for key in [
                "node.name",
                "media.class",
                "device.id",
                "device.description",
                "device.api",
                "audio.channels",
            ] {
                if let Some(value) = node.props.get(key) {
                    lines.push(Line::from(format!("{key}: {}", clean(value))));
                }
            }

            if let Some(profile) = snapshot
                .graph
                .profiles(node)
                .iter()
                .find(|profile| profile.current)
            {
                lines.push(Line::from(format!(
                    "Card profile: {}",
                    clean(&profile.name)
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), layout[1]);
    }
}

fn draw_picker(frame: &mut Frame, area: Rect, picker: &mut Picker) {
    let popup = Rect::new(
        area.x + 1,
        area.y + 2,
        area.width.saturating_sub(2),
        area.height.saturating_sub(4),
    );
    frame.render_widget(Clear, popup);
    let (title, items, selection) = match picker {
        Picker::Target {
            options, selection, ..
        } => (
            " Select target | Enter apply | Esc cancel ",
            options
                .iter()
                .map(|node| ListItem::new(clean(&node.name)))
                .collect::<Vec<_>>(),
            selection,
        ),
        Picker::Profile { options, selection } => (
            " Card profile | affects entire card | Enter apply | Esc cancel ",
            options
                .iter()
                .map(|profile| {
                    ListItem::new(format!(
                        "{}{}{}",
                        clean(&profile.name),
                        if profile.current { " [CURRENT]" } else { "" },
                        if profile.available {
                            ""
                        } else {
                            " [UNAVAILABLE]"
                        }
                    ))
                })
                .collect(),
            selection,
        ),
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(Color::Cyan)),
        popup,
        selection,
    );
}

fn row_line(row: &Row, snapshot: &Snapshot, width: usize) -> Line<'static> {
    let (volume, mixed) = common_volume(&row.nodes, &snapshot.volumes);
    let mut suffix = String::new();

    if row.nodes.first().is_some_and(Node::is_card) {
        suffix.push_str(" [NO AUDIO NODES]");
    } else if row.nodes.is_empty() {
        suffix.push_str(" inactive");
    } else if let Some(volume) = volume {
        if width >= 65 {
            let filled = (volume.level.clamp(0.0, 1.0) * 10.0).round() as usize;
            suffix.push_str(&format!(
                " [{}{}]",
                "█".repeat(filled),
                "-".repeat(10 - filled)
            ));
        }

        suffix.push_str(&format!(" {:.0}%", volume.level * 100.0));
        if volume.muted {
            suffix.push_str(" [MUTED]");
        }
        if mixed {
            suffix.push_str(" [MIXED]");
        }
    } else {
        suffix.push_str(" --%");
    }

    if row.persistent {
        suffix.push_str(" [PERSISTENT]");
    }
    if row
        .nodes
        .first()
        .is_some_and(|node| !node.is_stream() && snapshot.graph.is_default(node))
    {
        suffix.push_str(" [DEFAULT]");
    }
    if row.capture {
        suffix.push_str(" [RECORDING]");
    }

    let name_width = width.saturating_sub(suffix.chars().count()).max(8);
    let name: String = clean(&row.name).chars().take(name_width).collect();
    Line::from(vec![
        Span::raw(format!("{name:<name_width$}")),
        Span::raw(suffix),
    ])
}

fn references(nodes: &[Node]) -> Vec<NodeRef> {
    nodes.iter().map(|node| node.reference.clone()).collect()
}

fn select_valid(state: &mut ListState, selection: Option<usize>, length: usize) {
    state.select(if length == 0 {
        None
    } else {
        Some(selection.unwrap_or(0).min(length - 1))
    });
}

fn move_selection(state: &mut ListState, length: usize, down: bool) {
    if length == 0 {
        state.select(None);
        return;
    }
    let current = state.selected().unwrap_or(0);
    state.select(Some(if down {
        (current + 1).min(length - 1)
    } else {
        current.saturating_sub(1)
    }));
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
