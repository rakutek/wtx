//! ratatui コンソール: VMをプロジェクト（ホスト側リポジトリ）ごとにまとめて表示し、
//! 起動/停止・sync・削除・シェル起動を1画面で操作する。
use crate::lima::{self, Instance};
use crate::mirror;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

/// プロジェクト見出しか、その配下のVMか。
enum Row {
    Group { key: String, vms: usize, running: usize },
    Vm(Instance),
}

struct App {
    rows: Vec<Row>,
    collapsed: HashSet<String>,
    state: ListState,
    status: String,
    confirm_delete: bool,
    mirror_line: String,
    last_refresh: Instant,
}

const HELP: &str = "r:refresh  s:start/stop  y:sync  Enter:shell/fold  Space:fold  d:delete  q:quit";

impl App {
    fn new() -> Self {
        let mut a = App {
            rows: vec![],
            collapsed: HashSet::new(),
            state: ListState::default(),
            status: HELP.into(),
            confirm_delete: false,
            mirror_line: String::new(),
            last_refresh: Instant::now(),
        };
        a.refresh();
        a.state.select(Some(0));
        a
    }

    fn refresh(&mut self) {
        // プロジェクト（main_repo）ごとにまとめ、リポジトリを持たないVMは最後に置く
        let mut groups: BTreeMap<String, Vec<Instance>> = BTreeMap::new();
        for i in lima::list_instances() {
            groups.entry(i.repo.clone()).or_default().push(i);
        }
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort_by_key(|k| (k.is_empty(), k.clone()));

        self.rows.clear();
        for k in keys {
            let mut vms = groups.remove(&k).unwrap_or_default();
            vms.sort_by(|a, b| a.name.cmp(&b.name));
            let running = vms.iter().filter(|v| v.status == "Running").count();
            self.rows.push(Row::Group { key: k.clone(), vms: vms.len(), running });
            if !self.collapsed.contains(&k) {
                self.rows.extend(vms.into_iter().map(Row::Vm));
            }
        }
        if let Some(sel) = self.state.selected() {
            if sel >= self.rows.len() {
                self.state.select(self.rows.len().checked_sub(1));
            }
        }
        let mode = if crate::launchd::installed() { "launchd" } else { "manual" };
        let up: Vec<String> = mirror::mirror_config()
            .into_iter()
            .map(|e| {
                let mark = if mirror::port_alive(e.port) { "●" } else { "○" };
                format!("{mark}{}", e.registry)
            })
            .collect();
        self.mirror_line = format!("mirror[{mode}]  {}", up.join("  "));
        self.last_refresh = Instant::now();
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
        let i = self.state.selected().unwrap_or(0) as isize + delta;
        let i = i.clamp(0, self.rows.len() as isize - 1) as usize;
        self.state.select(Some(i));
    }

    /// 見出し行なら開閉する。VM行なら false を返す。
    fn toggle_group(&mut self) -> bool {
        let Some(Row::Group { key, .. }) = self.selected_row() else {
            return false;
        };
        let key = key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.refresh();
        true
    }
}

/// tty のない環境でも描画を確認できるスナップショット（CI・動作確認用）。
pub fn snapshot() -> Result<()> {
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 18))?;
    let mut app = App::new();
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
    let mut app = App::new();
    loop {
        term.draw(|f| draw(f, &mut app))?;

        if !event::poll(Duration::from_millis(500))? {
            if app.last_refresh.elapsed() > Duration::from_secs(5) {
                app.refresh();
            }
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if app.confirm_delete {
            if k.code == KeyCode::Char('y') {
                if let Some(name) = app.selected_vm().map(|i| i.name.clone()) {
                    app.status = format!("deleting {name}...");
                    term.draw(|f| draw(f, &mut app))?;
                    app.status = match lima::rm(&name) {
                        Ok(_) => format!("deleted {name}"),
                        Err(e) => format!("delete failed: {e}"),
                    };
                    app.refresh();
                }
            } else {
                app.status = "cancelled".into();
            }
            app.confirm_delete = false;
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => {
                app.refresh();
                app.status = "refreshed".into();
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                app.toggle_group();
            }
            KeyCode::Char('d') => {
                if app.selected_vm().is_some() {
                    app.confirm_delete = true;
                } else {
                    app.status = "select a VM first".into();
                }
            }
            KeyCode::Char('s') => {
                if let Some(inst) = app.selected_vm().cloned() {
                    let running = inst.status == "Running";
                    app.status =
                        format!("{}ing {}...", if running { "stopp" } else { "start" }, inst.name);
                    term.draw(|f| draw(f, &mut app))?;
                    let r = if running {
                        crate::util::limactl(&["stop", &inst.name])
                    } else {
                        crate::util::limactl(&["start", &inst.name, "--tty=false"])
                    };
                    app.status = match r {
                        Ok(_) => format!("{} done", inst.name),
                        Err(e) => format!("failed: {e}"),
                    };
                    app.refresh();
                } else {
                    app.status = "select a VM first".into();
                }
            }
            KeyCode::Char('y') => {
                if let Some(name) = app.selected_vm().map(|i| i.name.clone()) {
                    app.status = format!("syncing {name}...");
                    term.draw(|f| draw(f, &mut app))?;
                    app.status = match lima::sync(&name) {
                        Ok(_) => format!("{name}: fetched into refs/wtx/{name}/*"),
                        Err(e) => format!("sync failed: {e}"),
                    };
                } else {
                    app.status = "select a VM first".into();
                }
            }
            KeyCode::Enter => {
                if app.toggle_group() {
                    continue;
                }
                if let Some(name) = app.selected_vm().map(|i| i.name.clone()) {
                    // TUI を一旦畳んで対話シェルに入る
                    disable_raw_mode()?;
                    execute!(term.backend_mut(), LeaveAlternateScreen)?;
                    let r = crate::sshx::shell(&name);
                    enable_raw_mode()?;
                    execute!(term.backend_mut(), EnterAlternateScreen)?;
                    term.clear()?;
                    app.status = match r {
                        Ok(_) => format!("left the shell on {name}"),
                        Err(e) => format!("shell: {e}"),
                    };
                    app.refresh();
                }
            }
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .split(f.area());

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" wtx ", Style::new().bold().bg(Color::Cyan).fg(Color::Black)),
            Span::raw("  "),
            Span::styled(app.mirror_line.clone(), Style::new().fg(Color::DarkGray)),
        ])),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(format!(
            "    {:<24}{:<10}{:<10}{}",
            "NAME", "STATUS", "GIT", "BRANCH"
        ))
        .style(Style::new().bold().fg(Color::Yellow)),
        chunks[1],
    );

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| match row {
            Row::Group { key, vms, running } => {
                let mark = if app.collapsed.contains(key) { "▸" } else { "▾" };
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
                    Span::styled(format!("{mark} {label}"), Style::new().bold().fg(Color::Cyan)),
                    Span::styled(
                        format!("  [{running}/{vms} running]"),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]))
            }
            Row::Vm(i) => {
                let color = match i.status.as_str() {
                    "Running" => Color::Green,
                    "Stopped" => Color::DarkGray,
                    _ => Color::Yellow,
                };
                let git = if i.isolated {
                    "isolated"
                } else if i.workdir.is_empty() {
                    "-"
                } else {
                    "shared"
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("  {}", pad(&i.name, 24))),
                    Span::styled(pad(&i.status, 10), Style::new().fg(color)),
                    Span::raw(format!("{}{}", pad(git, 10), i.branch)),
                ]))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" VMs "))
        .highlight_style(Style::new().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("▶");
    f.render_stateful_widget(list, chunks[2], &mut app.state);

    let detail = match app.selected_row() {
        Some(Row::Vm(i)) => format!(
            "workdir : {}\nrepo    : {}\ngit     : {}",
            i.workdir,
            if i.repo.is_empty() { "-" } else { &i.repo },
            if i.isolated {
                "isolated (host .git stays untouched; press y to fetch commits)"
            } else {
                "shared or not a git repo"
            }
        ),
        Some(Row::Group { key, vms, running }) => {
            if key.is_empty() {
                format!("VMs not tied to a repository: {vms} ({running} running)")
            } else {
                format!(
                    "project : {}\nVMs     : {vms} ({running} running)\nnew VM  : wtx up <name> <worktree>",
                    key
                )
            }
        }
        None => "No VMs yet. Create one with `wtx up NAME WORKDIR`".to_string(),
    };
    f.render_widget(
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title(" detail ")),
        chunks[3],
    );

    f.render_widget(
        Paragraph::new(app.status.clone()).style(Style::new().fg(Color::DarkGray)),
        chunks[4],
    );

    if app.confirm_delete {
        let name = app.selected_vm().map(|i| i.name.clone()).unwrap_or_default();
        let area = centered(60, 5, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!(
                "\n Delete {name}? Its databases and images go with it.\n y = delete / any other key = cancel"
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" confirm ")
                    .border_style(Style::new().fg(Color::Red)),
            ),
            area,
        );
    }
}

/// 表示幅（全角=2）で右埋めする。`{:<n}` は文字数で数えるため、日本語ラベルで列がずれる。
fn pad(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

fn compact_path(p: &str) -> String {
    match dirs::home_dir() {
        Some(h) => p.replacen(&h.to_string_lossy().to_string(), "~", 1),
        None => p.to_string(),
    }
}

fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}
