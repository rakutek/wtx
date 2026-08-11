//! A ratatui console that groups VMs by project (host repository) and provides start, stop,
//! delete, shell, and wtx upgrade actions in one screen. Slow operations such as limactl
//! start/stop/delete, upgrades, and state collection run in background threads so the UI
//! remains responsive.
use crate::lima::{self, Instance};
use crate::mirror;
use crate::update::UpdateStatus;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// A project heading or one of its VMs.
enum Row {
    Group {
        key: String,
        vms: usize,
        running: usize,
    },
    Vm(Instance),
}

/// An action requiring confirmation to prevent accidental execution.
enum Confirm {
    Delete(String),
    Upgrade(String),
}

/// A notification from a background thread to the UI.
enum Msg {
    Refreshed {
        instances: Vec<Instance>,
        sim_states: BTreeMap<String, String>,
        mirror_line: String,
    },
    OpDone {
        name: String,
        verb: &'static str,
        result: std::result::Result<(), String>,
    },
    UpdateChecked(UpdateStatus),
    UpgradeDone {
        version: String,
        result: std::result::Result<(), String>,
    },
}

// Column widths. The first four columns align with the border (1), selection marker (1),
// and VM-row indentation (2).
const W_NAME: usize = 24;
const W_STATUS: usize = 14;
const W_BRANCH: usize = 16;
const W_SIM: usize = 12;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const HELP: &str =
    " j/k:move  Enter:shell/fold  s:start/stop  d:delete  u:upgrade  Space:fold  r:refresh  q:quit";

struct App {
    instances: Vec<Instance>,
    rows: Vec<Row>,
    collapsed: HashSet<String>,
    state: ListState,
    status: String,
    status_err: bool,
    /// Pending confirmation. Capture the target on keypress so state changes cannot alter it.
    confirm: Option<Confirm>,
    mirror_line: String,
    last_refresh: Instant,
    /// Worktree simulator states (UDID -> Booted/Shutdown), empty when no VM uses a simulator.
    sim_states: BTreeMap<String, String>,
    /// Active operations (VM name -> (verb, start time)), used to prevent duplicate actions.
    in_flight: HashMap<String, (&'static str, Instant)>,
    refreshing: bool,
    update_available: Option<UpdateStatus>,
    /// Target upgrade version and start time, used to prevent duplicate runs and early exit.
    upgrading: Option<(String, Instant)>,
    tx: Sender<Msg>,
}

/// Collect the VM list, simulator states, and mirror row. This is slow because it invokes
/// subprocesses.
fn fetch_state() -> (Vec<Instance>, BTreeMap<String, String>, String) {
    let instances = lima::list_instances();
    // Query simctl only when a VM uses a simulator, avoiding xcrun on unsupported systems.
    let sim_states = crate::sim::states_for(
        &instances
            .iter()
            .filter(|i| !i.sim_udid.is_empty())
            .map(|i| i.sim_udid.clone())
            .collect::<Vec<_>>(),
    );
    let mode = if crate::launchd::installed() {
        "launchd"
    } else {
        "manual"
    };
    let up: Vec<String> = mirror::mirror_config()
        .into_iter()
        .map(|e| {
            let mark = if mirror::port_alive(e.port) {
                "●"
            } else {
                "○"
            };
            format!("{mark}{}", e.registry)
        })
        .collect();
    (
        instances,
        sim_states,
        format!("mirror[{mode}]  {}", up.join("  ")),
    )
}

impl App {
    fn new(tx: Sender<Msg>, update_available: Option<UpdateStatus>) -> Self {
        let mut a = Self {
            instances: vec![],
            rows: vec![],
            collapsed: HashSet::new(),
            state: ListState::default(),
            status: String::new(),
            status_err: false,
            confirm: None,
            mirror_line: String::new(),
            last_refresh: Instant::now(),
            sim_states: BTreeMap::new(),
            in_flight: HashMap::new(),
            refreshing: false,
            update_available,
            upgrading: None,
            tx,
        };
        // Collect synchronously only at startup so the first frame is populated. `--snapshot`
        // also relies on this behavior.
        let (i, s, m) = fetch_state();
        a.apply(i, s, m);
        if a.state.selected().is_none() && !a.rows.is_empty() {
            a.state.select(Some(0));
        }
        a
    }

    fn apply(
        &mut self,
        instances: Vec<Instance>,
        sim_states: BTreeMap<String, String>,
        mirror_line: String,
    ) {
        self.instances = instances;
        self.sim_states = sim_states;
        self.mirror_line = mirror_line;
        self.last_refresh = Instant::now();
        self.rebuild_rows();
    }

    /// Build display rows from instances. Restore selection by item identity rather than row
    /// number so background refreshes cannot make the cursor jump to another item.
    fn rebuild_rows(&mut self) {
        let prev = self.selected_key();
        let mut groups: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
        for i in self.instances.clone() {
            groups.entry(i.repo.clone()).or_default().push(i);
        }
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort_by_key(|k| (k.is_empty(), k.clone()));

        self.rows.clear();
        for k in keys {
            let mut vms = groups.remove(&k).unwrap_or_default();
            vms.sort_by(|a, b| a.name.cmp(&b.name));
            let running = vms.iter().filter(|v| v.status == "Running").count();
            self.rows.push(Row::Group {
                key: k.clone(),
                vms: vms.len(),
                running,
            });
            if !self.collapsed.contains(&k) {
                self.rows.extend(vms.into_iter().map(Row::Vm));
            }
        }

        let idx = prev.and_then(|key| {
            self.rows.iter().position(|r| match (r, &key) {
                (Row::Group { key: k, .. }, (true, name)) => k == name,
                (Row::Vm(i), (false, name)) => &i.name == name,
                _ => false,
            })
        });
        if let Some(i) = idx {
            self.state.select(Some(i));
        } else {
            let cur = self.state.selected().unwrap_or(0);
            self.state
                .select(self.rows.len().checked_sub(1).map(|last| cur.min(last)));
        }
    }

    fn selected_key(&self) -> Option<(bool, String)> {
        self.selected_row().map(|r| match r {
            Row::Group { key, .. } => (true, key.clone()),
            Row::Vm(i) => (false, i.name.clone()),
        })
    }

    fn selected_row(&self) -> Option<&Row> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    fn selected_vm(&self) -> Option<&Instance> {
        match self.selected_row() {
            Some(Row::Vm(i)) => Some(i),
            _ => None,
        }
    }

    fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let i = self
            .state
            .selected()
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        self.state.select(Some(i));
    }

    /// Toggle a heading row. Return false for a VM row.
    fn toggle_group(&mut self) -> bool {
        let Some(Row::Group { key, .. }) = self.selected_row() else {
            return false;
        };
        let key = key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_rows();
        true
    }

    fn spawn_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (instances, sim_states, mirror_line) = fetch_state();
            let _ = tx.send(Msg::Refreshed {
                instances,
                sim_states,
                mirror_line,
            });
        });
    }

    /// Run start, stop, or delete in the background and return to the UI immediately.
    fn spawn_op(&mut self, name: String, verb: &'static str) {
        self.in_flight.insert(name.clone(), (verb, Instant::now()));
        self.set_status(format!("{verb} {name} …"), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match verb {
                "start" => crate::util::limactl_capture(&["start", &name, "--tty=false"]),
                "stop" => lima::stop(&name, true),
                _ => lima::rm(&name, lima::RemoveOpts::default()).map_err(|e| e.to_string()),
            };
            let _ = tx.send(Msg::OpDone { name, verb, result });
        });
    }

    fn prompt_upgrade(&mut self) {
        if let Some((version, _)) = &self.upgrading {
            self.set_status(
                format!("upgrade to v{version} is already in progress"),
                false,
            );
            return;
        }
        let Some(update) = &self.update_available else {
            self.set_status("no wtx update available".into(), false);
            return;
        };
        self.confirm = Some(Confirm::Upgrade(update.latest_version.clone()));
    }

    /// Capture Homebrew output during upgrade to preserve the alternate screen and return the
    /// result to the UI.
    fn spawn_upgrade(&mut self, version: String) {
        self.upgrading = Some((version.clone(), Instant::now()));
        self.set_status(format!("upgrading wtx to v{version} …"), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = crate::update::upgrade_captured(&version).map_err(|e| e.to_string());
            let _ = tx.send(Msg::UpgradeDone { version, result });
        });
    }

    fn set_status(&mut self, s: String, err: bool) {
        self.status = s;
        self.status_err = err;
    }

    fn busy(&self, name: &str) -> bool {
        self.in_flight.contains_key(name)
    }

    /// `need_clear` is true when the next frame should erase output written by a child process.
    fn handle(&mut self, m: Msg, need_clear: &mut bool) {
        match m {
            Msg::Refreshed {
                instances,
                sim_states,
                mirror_line,
            } => {
                self.refreshing = false;
                self.apply(instances, sim_states, mirror_line);
            }
            Msg::OpDone { name, verb, result } => {
                self.in_flight.remove(&name);
                match result {
                    Ok(()) => self.set_status(format!("{verb} {name}: done"), false),
                    Err(e) => self.set_status(format!("{verb} {name} failed: {e}"), true),
                }
                *need_clear = true;
                self.refreshing = false;
                self.spawn_refresh();
            }
            Msg::UpdateChecked(status) => {
                self.update_available = status.update_available.then_some(status);
            }
            Msg::UpgradeDone { version, result } => {
                self.upgrading = None;
                match result {
                    Ok(()) => {
                        self.update_available = None;
                        self.set_status(
                            format!("Homebrew upgrade finished for v{version} — restart wtx"),
                            false,
                        );
                    }
                    Err(e) => self.set_status(format!("upgrade failed: {e}"), true),
                }
            }
        }
    }
}

/// Render a snapshot without a TTY for CI and manual verification.
pub fn snapshot() -> Result<()> {
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 18))?;
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(tx, None);
    term.draw(|f| draw(f, &mut app))?;
    let buf = term.backend().buffer().clone();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let res = event_loop(&mut term);
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop<B: Backend + std::io::Write>(term: &mut Terminal<B>) -> Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let (cached_update, should_check) = crate::update::tui_state();
    let mut app = App::new(tx.clone(), cached_update);
    if should_check {
        std::thread::spawn(move || {
            if let Ok(status) = crate::update::check() {
                let _ = tx.send(Msg::UpdateChecked(status));
            }
        });
    }
    loop {
        let mut need_clear = false;
        while let Ok(m) = rx.try_recv() {
            app.handle(m, &mut need_clear);
        }
        if need_clear {
            term.clear()?;
        }
        term.draw(|f| draw(f, &mut app))?;

        // Poll in 150 ms increments to animate spinners while waiting for input. On timeout,
        // perform only the periodic refresh.
        if !event::poll(Duration::from_millis(150))? {
            if app.last_refresh.elapsed() > Duration::from_secs(5) {
                app.spawn_refresh();
            }
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }

        // The confirmation modal acts only on the target captured when the key was pressed.
        if let Some(confirm) = app.confirm.take() {
            match (confirm, k.code) {
                (Confirm::Delete(name), KeyCode::Char('y')) => app.spawn_op(name, "delete"),
                (Confirm::Upgrade(version), KeyCode::Char('y')) => app.spawn_upgrade(version),
                _ => app.set_status("cancelled".into(), false),
            }
            continue;
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if app.upgrading.is_some() {
                    app.set_status("upgrade in progress — wait before quitting".into(), false);
                } else {
                    return Ok(());
                }
            }
            KeyCode::Char('r') => {
                app.spawn_refresh();
                app.set_status("refreshing…".into(), false);
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
            KeyCode::Char('g') => app.state.select(app.rows.first().map(|_| 0)),
            KeyCode::Char('G') => app.state.select(app.rows.len().checked_sub(1)),
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                app.toggle_group();
            }
            KeyCode::Char('d') => match app.selected_vm().map(|i| i.name.clone()) {
                Some(name) if app.busy(&name) => {
                    app.set_status(format!("{name}: operation in progress"), false);
                }
                Some(name) => app.confirm = Some(Confirm::Delete(name)),
                None => app.set_status("select a VM first".into(), false),
            },
            KeyCode::Char('u') => app.prompt_upgrade(),
            KeyCode::Char('s') => match app.selected_vm().cloned() {
                Some(i) if app.busy(&i.name) => {
                    app.set_status(format!("{}: operation in progress", i.name), false);
                }
                Some(i) => {
                    let verb = if i.status == "Running" {
                        "stop"
                    } else {
                        "start"
                    };
                    app.spawn_op(i.name, verb);
                }
                None => app.set_status("select a VM first".into(), false),
            },
            KeyCode::Enter => {
                if app.toggle_group() {
                    continue;
                }
                let Some(i) = app.selected_vm().cloned() else {
                    continue;
                };
                if app.busy(&i.name) {
                    app.set_status(format!("{}: operation in progress", i.name), false);
                    continue;
                }
                if i.status != "Running" {
                    app.set_status(
                        format!("{} is not running — press s to start it", i.name),
                        false,
                    );
                    continue;
                }
                // Temporarily suspend the TUI while entering an interactive shell.
                disable_raw_mode()?;
                execute!(term.backend_mut(), LeaveAlternateScreen)?;
                let r = crate::sshx::shell(&i.name);
                enable_raw_mode()?;
                execute!(term.backend_mut(), EnterAlternateScreen)?;
                term.clear()?;
                match r {
                    Ok(()) => app.set_status(format!("left the shell on {}", i.name), false),
                    Err(e) => app.set_status(format!("shell: {e}"), true),
                }
                app.spawn_refresh();
            }
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // Title
        Constraint::Length(1), // Column headers
        Constraint::Min(5),    // VM list
        Constraint::Length(6), // Details
        Constraint::Length(1), // Status and operation results
        Constraint::Length(1), // Persistent key help
    ])
    .split(f.area());

    let mut title = vec![Span::styled(
        " wtx ",
        Style::new().bold().bg(Color::Cyan).fg(Color::Black),
    )];
    if let Some((version, since)) = &app.upgrading {
        let spinner = SPINNER[(since.elapsed().as_millis() / 120) as usize % SPINNER.len()];
        title.push(Span::styled(
            format!(
                "  {spinner} upgrading to v{version} ({}s)",
                since.elapsed().as_secs()
            ),
            Style::new().fg(Color::Yellow),
        ));
    } else if let Some(update) = &app.update_available {
        title.push(Span::styled(
            format!(
                "  ↑ v{} available — press u to upgrade",
                update.latest_version
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    title.push(Span::raw("  "));
    title.push(Span::styled(
        app.mirror_line.clone(),
        Style::new().fg(Color::DarkGray),
    ));
    if app.refreshing {
        title.push(Span::styled("  ⟳", Style::new().fg(Color::DarkGray)));
    }
    f.render_widget(Paragraph::new(Line::from(title)), chunks[0]);

    f.render_widget(
        Paragraph::new(format!(
            "    {}{}{}{}NOTE",
            fit("NAME", W_NAME),
            fit("STATUS", W_STATUS),
            fit("BRANCH", W_BRANCH),
            fit("SIM", W_SIM),
        ))
        .style(Style::new().bold().fg(Color::Yellow)),
        chunks[1],
    );

    let block = Block::default().borders(Borders::ALL).title(" VMs ");
    if app.rows.is_empty() {
        let inner = block.inner(chunks[2]);
        f.render_widget(block, chunks[2]);
        let msg = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(2) / 2,
            width: inner.width,
            height: 2.min(inner.height),
        };
        f.render_widget(
            Paragraph::new("No VMs yet.\nCreate one with:  wtx up NAME WORKDIR")
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::DarkGray)),
            msg,
        );
    } else {
        let items: Vec<ListItem> = app.rows.iter().map(|row| render_row(app, row)).collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Blue).fg(Color::White))
            .highlight_symbol("▶");
        f.render_stateful_widget(list, chunks[2], &mut app.state);
    }

    let detail = match app.selected_row() {
        Some(Row::Vm(i)) => {
            let mut d = format!(
                "workdir : {}\nrepo    : {}",
                i.workdir,
                if i.repo.is_empty() { "-" } else { &i.repo }
            );
            if !i.sim_udid.is_empty() {
                let st = app
                    .sim_states
                    .get(&i.sim_udid)
                    .map_or("missing", String::as_str);
                let _ = write!(d, "\nsim     : {} ({st})", i.sim_udid);
            }
            if let Some((verb, since)) = app.in_flight.get(&i.name) {
                let _ = write!(
                    d,
                    "\nop      : {verb} in progress ({}s)",
                    since.elapsed().as_secs()
                );
            }
            if i.orphaned {
                d.push_str(
                    "\nORPHANED: the worktree is gone (commits are on the host; delete when done)",
                );
            }
            d
        }
        Some(Row::Group { key, vms, running }) => {
            if key.is_empty() {
                format!("VMs not tied to a repository: {vms} ({running} running)")
            } else {
                format!(
                    "project : {key}\nVMs     : {vms} ({running} running)\nnew VM  : wtx up <name> <worktree>"
                )
            }
        }
        None => "No VMs yet. Create one with `wtx up NAME WORKDIR`".to_string(),
    };
    f.render_widget(
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title(" detail ")),
        chunks[3],
    );

    let status_style = if app.status_err {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::White)
    };
    f.render_widget(
        Paragraph::new(format!(" {}", app.status)).style(status_style),
        chunks[4],
    );
    f.render_widget(
        Paragraph::new(HELP).style(Style::new().fg(Color::DarkGray)),
        chunks[5],
    );

    if let Some(confirm) = &app.confirm {
        match confirm {
            Confirm::Delete(name) => {
                let area = centered(64, 7, f.area());
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(vec![
                        Line::raw(""),
                        Line::from(vec![
                            Span::raw(" Delete "),
                            Span::styled(name, Style::new().bold()),
                            Span::raw(" ?"),
                        ]),
                        Line::raw(" Its databases and images go with it."),
                        Line::raw(""),
                        Line::from(Span::styled(
                            " y = delete    any other key = cancel",
                            Style::new().fg(Color::DarkGray),
                        )),
                    ])
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" confirm delete ")
                            .border_style(Style::new().fg(Color::Red)),
                    ),
                    area,
                );
            }
            Confirm::Upgrade(version) => {
                let area = centered(68, 7, f.area());
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(vec![
                        Line::raw(""),
                        Line::from(vec![
                            Span::raw(" Upgrade wtx to "),
                            Span::styled(format!("v{version}"), Style::new().bold()),
                            Span::raw(" ?"),
                        ]),
                        Line::raw(" Homebrew metadata and the wtx formula will be updated."),
                        Line::raw(""),
                        Line::from(Span::styled(
                            " y = upgrade    any other key = cancel",
                            Style::new().fg(Color::DarkGray),
                        )),
                    ])
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" confirm upgrade ")
                            .border_style(Style::new().fg(Color::Yellow)),
                    ),
                    area,
                );
            }
        }
    }
}

fn render_row<'a>(app: &App, row: &'a Row) -> ListItem<'a> {
    match row {
        Row::Group { key, vms, running } => {
            let mark = if app.collapsed.contains(key) {
                "▸"
            } else {
                "▾"
            };
            let label = if key.is_empty() {
                "(no project)".to_string()
            } else {
                format!(
                    "{}  {}",
                    std::path::Path::new(key)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    compact_path(key)
                )
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{mark} {label}"),
                    Style::new().bold().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("  [{running}/{vms} running]"),
                    Style::new().fg(Color::DarkGray),
                ),
            ]))
        }
        Row::Vm(i) => {
            let mut spans = vec![Span::raw(format!("  {}", fit(&i.name, W_NAME)))];
            if let Some((verb, since)) = app.in_flight.get(&i.name) {
                let sp = SPINNER[(since.elapsed().as_millis() / 120) as usize % SPINNER.len()];
                spans.push(Span::styled(
                    fit(
                        &format!("{sp} {verb} {}s", since.elapsed().as_secs()),
                        W_STATUS,
                    ),
                    Style::new().fg(Color::Yellow),
                ));
            } else {
                let color = match i.status.as_str() {
                    "Running" => Color::Green,
                    "Stopped" => Color::DarkGray,
                    _ => Color::Yellow,
                };
                spans.push(Span::styled(
                    fit(&i.status, W_STATUS),
                    Style::new().fg(color),
                ));
            }
            spans.push(Span::raw(fit(&i.branch, W_BRANCH)));
            if i.sim_udid.is_empty() {
                spans.push(Span::raw(" ".repeat(W_SIM)));
            } else {
                let st = app
                    .sim_states
                    .get(&i.sim_udid)
                    .map_or("missing", String::as_str);
                let c = match st {
                    "Booted" => Color::Green,
                    "Shutdown" => Color::DarkGray,
                    _ => Color::Yellow,
                };
                spans.push(Span::styled(
                    fit(&format!("sim:{st}"), W_SIM),
                    Style::new().fg(c),
                ));
            }
            if i.orphaned {
                spans.push(Span::styled("orphaned", Style::new().fg(Color::Red).bold()));
            }
            ListItem::new(Line::from(spans))
        }
    }
}

/// Fit text to a display width, counting full-width characters as two columns and truncating
/// with `…` when needed.
fn fit(s: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let w = UnicodeWidthStr::width(s);
    if w <= width {
        return format!("{s}{}", " ".repeat(width - w));
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > width.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    format!("{out}…{}", " ".repeat(width.saturating_sub(used + 1)))
}

fn compact_path(p: &str) -> String {
    dirs::home_dir().map_or_else(
        || p.to_string(),
        |home| p.replacen(home.to_string_lossy().as_ref(), "~", 1),
    )
}

fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available_update(version: &str) -> UpdateStatus {
        UpdateStatus {
            schema_version: 1,
            current_version: "0.9.0".into(),
            latest_version: version.into(),
            update_available: true,
            release_url: "https://example.test/release".into(),
        }
    }

    fn app(update_available: Option<UpdateStatus>) -> App {
        let (tx, _rx) = mpsc::channel();
        App {
            instances: vec![],
            rows: vec![],
            collapsed: HashSet::new(),
            state: ListState::default(),
            status: String::new(),
            status_err: false,
            confirm: None,
            mirror_line: "mirror[manual]".into(),
            last_refresh: Instant::now(),
            sim_states: BTreeMap::new(),
            in_flight: HashMap::new(),
            refreshing: false,
            update_available,
            upgrading: None,
            tx,
        }
    }

    fn rendered(app: &mut App) -> String {
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 18)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buffer = term.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        rendered
    }

    #[test]
    fn update_notice_is_actionable_from_the_tui() {
        let mut app = app(Some(available_update("1.0.0")));

        assert!(rendered(&mut app).contains("v1.0.0 available — press u to upgrade"));
        app.prompt_upgrade();
        assert!(matches!(
            app.confirm.as_ref(),
            Some(Confirm::Upgrade(version)) if version == "1.0.0"
        ));
        assert!(rendered(&mut app).contains("Upgrade wtx to v1.0.0 ?"));
    }

    #[test]
    fn successful_upgrade_clears_notice_and_requests_restart() {
        let mut app = app(Some(available_update("1.0.0")));
        app.upgrading = Some(("1.0.0".into(), Instant::now()));
        let mut need_clear = false;

        app.handle(
            Msg::UpgradeDone {
                version: "1.0.0".into(),
                result: Ok(()),
            },
            &mut need_clear,
        );

        assert!(app.upgrading.is_none());
        assert!(app.update_available.is_none());
        assert!(!app.status_err);
        assert!(app.status.contains("restart wtx"));
    }

    #[test]
    fn failed_upgrade_keeps_notice_for_retry() {
        let mut app = app(Some(available_update("1.0.0")));
        app.upgrading = Some(("1.0.0".into(), Instant::now()));
        let mut need_clear = false;

        app.handle(
            Msg::UpgradeDone {
                version: "1.0.0".into(),
                result: Err("brew failed".into()),
            },
            &mut need_clear,
        );

        assert!(app.upgrading.is_none());
        assert!(app.update_available.is_some());
        assert!(app.status_err);
        assert!(app.status.contains("brew failed"));
    }
}
