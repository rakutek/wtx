//! ratatui コンソール: VMをプロジェクト（ホスト側リポジトリ）ごとにまとめて表示し、
//! 起動/停止・削除・シェル起動を1画面で操作する。
//! 時間のかかる処理（limactl start/stop/delete と状態の取得）はバックグラウンド
//! スレッドで実行し、UIは操作中も止まらない。
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// プロジェクト見出しか、その配下のVMか。
enum Row {
    Group {
        key: String,
        vms: usize,
        running: usize,
    },
    Vm(Instance),
}

/// バックグラウンドスレッドからUIへの通知。
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
}

// 列幅。ヘッダー行の先頭4桁 = 枠(1) + 選択記号(1) + VM行の字下げ(2) に合わせてある。
const W_NAME: usize = 24;
const W_STATUS: usize = 14;
const W_BRANCH: usize = 16;
const W_SIM: usize = 12;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const HELP: &str =
    " j/k:move  Enter:shell/fold  s:start/stop  d:delete  Space:fold  r:refresh  q:quit";

struct App {
    instances: Vec<Instance>,
    rows: Vec<Row>,
    collapsed: HashSet<String>,
    state: ListState,
    status: String,
    status_err: bool,
    /// 削除確認中のVM名。d押下時に固定し、確認中に行がずれても対象は変わらない。
    confirm: Option<String>,
    mirror_line: String,
    last_refresh: Instant,
    /// worktree専用シミュレータの状態（UDID → Booted/Shutdown）。sim を使うVMが無ければ空。
    sim_states: BTreeMap<String, String>,
    /// 実行中の操作（VM名 → (verb, 開始時刻)）。対象VMへの二重操作を防ぐ。
    in_flight: HashMap<String, (&'static str, Instant)>,
    refreshing: bool,
    tx: Sender<Msg>,
}

/// VM一覧・sim状態・ミラー行をまとめて取得する（サブプロセス呼び出しを含むため遅い）。
fn fetch_state() -> (Vec<Instance>, BTreeMap<String, String>, String) {
    let instances = lima::list_instances();
    // sim を使うVMがあるときだけ simctl に問い合わせる（xcrun 無し環境を巻き込まない）
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
    fn new(tx: Sender<Msg>) -> Self {
        let mut a = App {
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
            tx,
        };
        // 初回だけ同期で取得し、最初のフレームから中身を出す（--snapshot もこれに依存）
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

    /// instances から表示行を組み立てる。選択は行番号でなく「何を選んでいたか」で
    /// 復元する（バックグラウンド更新で行がずれてもカーソルが飛ばない）。
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
        match idx {
            Some(i) => self.state.select(Some(i)),
            None => {
                let cur = self.state.selected().unwrap_or(0);
                self.state
                    .select(self.rows.len().checked_sub(1).map(|last| cur.min(last)));
            }
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

    /// start/stop/delete をバックグラウンドで実行する。終了を待たずにUIへ戻る。
    fn spawn_op(&mut self, name: String, verb: &'static str) {
        self.in_flight.insert(name.clone(), (verb, Instant::now()));
        self.set_status(format!("{verb} {name} …"), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match verb {
                "start" => crate::util::limactl_capture(&["start", &name, "--tty=false"]),
                "stop" => crate::util::limactl_capture(&["stop", &name]),
                _ => lima::rm(&name, false).map_err(|e| e.to_string()),
            };
            let _ = tx.send(Msg::OpDone { name, verb, result });
        });
    }

    fn set_status(&mut self, s: String, err: bool) {
        self.status = s;
        self.status_err = err;
    }

    fn busy(&self, name: &str) -> bool {
        self.in_flight.contains_key(name)
    }

    /// need_clear: 子プロセスが画面に書いた行を次フレームで消したいとき true。
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
        }
    }
}

/// tty のない環境でも描画を確認できるスナップショット（CI・動作確認用）。
pub fn snapshot() -> Result<()> {
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 18))?;
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(tx);
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
    let mut app = App::new(tx);
    loop {
        let mut need_clear = false;
        while let Ok(m) = rx.try_recv() {
            app.handle(m, &mut need_clear);
        }
        if need_clear {
            term.clear()?;
        }
        term.draw(|f| draw(f, &mut app))?;

        // 150ms 刻み: スピナーを回しつつキー入力を待つ。タイムアウト時は定期更新のみ。
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

        // 削除確認モーダル: d押下時に固定した名前に対してだけ働く
        if let Some(name) = app.confirm.take() {
            if k.code == KeyCode::Char('y') {
                app.spawn_op(name, "delete");
            } else {
                app.set_status("cancelled".into(), false);
            }
            continue;
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
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
                    app.set_status(format!("{name}: operation in progress"), false)
                }
                Some(name) => app.confirm = Some(name),
                None => app.set_status("select a VM first".into(), false),
            },
            KeyCode::Char('s') => match app.selected_vm().cloned() {
                Some(i) if app.busy(&i.name) => {
                    app.set_status(format!("{}: operation in progress", i.name), false)
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
                // TUI を一旦畳んで対話シェルに入る
                disable_raw_mode()?;
                execute!(term.backend_mut(), LeaveAlternateScreen)?;
                let r = crate::sshx::shell(&i.name);
                enable_raw_mode()?;
                execute!(term.backend_mut(), EnterAlternateScreen)?;
                term.clear()?;
                match r {
                    Ok(_) => app.set_status(format!("left the shell on {}", i.name), false),
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
        Constraint::Length(1), // タイトル
        Constraint::Length(1), // 列ヘッダー
        Constraint::Min(5),    // VM一覧
        Constraint::Length(6), // 詳細
        Constraint::Length(1), // ステータス（操作結果）
        Constraint::Length(1), // キー操作ヘルプ（常設）
    ])
    .split(f.area());

    let mut title = vec![
        Span::styled(
            " wtx ",
            Style::new().bold().bg(Color::Cyan).fg(Color::Black),
        ),
        Span::raw("  "),
        Span::styled(app.mirror_line.clone(), Style::new().fg(Color::DarkGray)),
    ];
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
                    .map(String::as_str)
                    .unwrap_or("missing");
                d.push_str(&format!("\nsim     : {} ({st})", i.sim_udid));
            }
            if let Some((verb, since)) = app.in_flight.get(&i.name) {
                d.push_str(&format!(
                    "\nop      : {verb} in progress ({}s)",
                    since.elapsed().as_secs()
                ));
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

    if let Some(name) = app.confirm.clone() {
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
                    .map(String::as_str)
                    .unwrap_or("missing");
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

/// 表示幅（全角=2）で width にそろえる。収まらなければ … で切り詰める。
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
    match dirs::home_dir() {
        Some(h) => p.replacen(&h.to_string_lossy().to_string(), "~", 1),
        None => p.to_string(),
    }
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
