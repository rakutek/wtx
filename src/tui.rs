//! ratatui コンソール: VM一覧・状態・ミラーを一画面で見て操作する。
use crate::lima::{self, Instance};
use crate::mirror;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use std::time::{Duration, Instant};

struct App {
    rows: Vec<Instance>,
    state: TableState,
    status: String,
    confirm_delete: bool,
    mirror_line: String,
    last_refresh: Instant,
}

impl App {
    fn new() -> Self {
        let mut a = App {
            rows: vec![],
            state: TableState::default(),
            status: "r:更新  s:起動/停止  y:sync  Enter:shell  d:削除  q:終了".into(),
            confirm_delete: false,
            mirror_line: String::new(),
            last_refresh: Instant::now(),
        };
        a.refresh();
        a.state.select(Some(0));
        a
    }

    fn refresh(&mut self) {
        self.rows = lima::list_instances();
        let mode = if crate::launchd::installed() { "launchd" } else { "手動" };
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

    fn selected(&self) -> Option<&Instance> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0) as isize + delta;
        let i = i.clamp(0, self.rows.len() as isize - 1) as usize;
        self.state.select(Some(i));
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
            match k.code {
                KeyCode::Char('y') => {
                    if let Some(name) = app.selected().map(|i| i.name.clone()) {
                        app.status = format!("{name} を削除中...");
                        term.draw(|f| draw(f, &mut app))?;
                        app.status = match lima::rm(&name) {
                            Ok(_) => format!("{name} を削除しました"),
                            Err(e) => format!("削除失敗: {e}"),
                        };
                        app.refresh();
                    }
                    app.confirm_delete = false;
                }
                _ => {
                    app.confirm_delete = false;
                    app.status = "キャンセルしました".into();
                }
            }
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => {
                app.refresh();
                app.status = "更新しました".into();
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
            KeyCode::Char('d') => {
                if app.selected().is_some() {
                    app.confirm_delete = true;
                }
            }
            KeyCode::Char('s') => {
                if let Some(inst) = app.selected().cloned() {
                    let running = inst.status == "Running";
                    app.status = format!("{} を{}中...", inst.name, if running { "停止" } else { "起動" });
                    term.draw(|f| draw(f, &mut app))?;
                    let r = if running {
                        crate::util::limactl(&["stop", &inst.name])
                    } else {
                        crate::util::limactl(&["start", &inst.name, "--tty=false"])
                    };
                    app.status = match r {
                        Ok(_) => format!("{} 完了", inst.name),
                        Err(e) => format!("失敗: {e}"),
                    };
                    app.refresh();
                }
            }
            KeyCode::Char('y') => {
                if let Some(name) = app.selected().map(|i| i.name.clone()) {
                    app.status = format!("{name} を sync 中...");
                    term.draw(|f| draw(f, &mut app))?;
                    app.status = match lima::sync(&name) {
                        Ok(_) => format!("{name}: refs/wtx/{name}/* に回収しました"),
                        Err(e) => format!("sync 失敗: {e}"),
                    };
                }
            }
            KeyCode::Enter => {
                if let Some(name) = app.selected().map(|i| i.name.clone()) {
                    // TUI を一旦畳んで対話シェルに入る
                    disable_raw_mode()?;
                    execute!(term.backend_mut(), LeaveAlternateScreen)?;
                    let r = crate::sshx::shell(&name);
                    enable_raw_mode()?;
                    execute!(term.backend_mut(), EnterAlternateScreen)?;
                    term.clear()?;
                    app.status = match r {
                        Ok(_) => format!("{name} のシェルを終了しました"),
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

    let header = Row::new(vec!["NAME", "STATUS", "GIT", "BRANCH", "WORKDIR"])
        .style(Style::new().bold().fg(Color::Yellow));
    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|i| {
            let color = match i.status.as_str() {
                "Running" => Color::Green,
                "Stopped" => Color::DarkGray,
                _ => Color::Yellow,
            };
            let git = if i.isolated { "隔離" } else if i.workdir.is_empty() { "-" } else { "共有" };
            Row::new(vec![
                Cell::from(i.name.clone()),
                Cell::from(i.status.clone()).style(Style::new().fg(color)),
                Cell::from(git),
                Cell::from(i.branch.clone()),
                Cell::from(compact_path(&i.workdir)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(16),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" VMs "))
    .row_highlight_style(Style::new().bg(Color::Blue).fg(Color::White))
    .highlight_symbol("▶ ");
    f.render_stateful_widget(table, chunks[1], &mut app.state);

    let detail = match app.selected() {
        Some(i) => {
            let meta = lima::load_meta(&i.name);
            let repo = meta.as_ref().map(|m| m.main_repo.clone()).unwrap_or_default();
            format!(
                "workdir : {}\nrepo    : {}\ngit     : {}",
                i.workdir,
                if repo.is_empty() { "-" } else { &repo },
                if i.isolated {
                    "隔離git（ホストの .git は不変。回収は y キー）"
                } else {
                    "共有 or 非git"
                }
            )
        }
        None => "VMがありません。`wtx up NAME WORKDIR` で作成します".to_string(),
    };
    f.render_widget(
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title(" detail ")),
        chunks[2],
    );

    f.render_widget(
        Paragraph::new(app.status.clone()).style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );

    if app.confirm_delete {
        let name = app.selected().map(|i| i.name.clone()).unwrap_or_default();
        let area = centered(60, 5, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!(
                "\n {name} を削除します（DB・イメージも消えます）\n y = 実行 / その他 = 中止"
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 確認 ")
                    .border_style(Style::new().fg(Color::Red)),
            ),
            area,
        );
    }
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
