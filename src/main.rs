use std::{
    io::{self, BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph},
};
use serde::Deserialize;
use serde_json::{Value, json};

const API: &str = "https://api.audius.co/v1";

#[derive(Clone, Debug, Deserialize)]
struct Artist {
    name: String,
}
#[derive(Clone, Debug, Deserialize)]
struct Track {
    id: String,
    title: String,
    user: Artist,
    duration: u64,
    #[serde(default)]
    is_stream_gated: bool,
    #[serde(default)]
    is_streamable: bool,
    #[serde(default)]
    is_available: bool,
}
impl Track {
    fn label(&self) -> String {
        format!(
            "{} — {}  {}",
            self.title,
            self.user.name,
            clock(self.duration as f64)
        )
    }
}
#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<Track>,
}
fn search(query: &str) -> Result<Vec<Track>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("{API}/tracks/search"))
        .query(&[("query", query), ("limit", "50"), ("app_name", "tusic")])
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json::<SearchResponse>())
        .map_err(|e| format!("Busca falhou: {e}"))?;
    Ok(response
        .data
        .into_iter()
        .filter(|t| t.is_streamable && t.is_available && !t.is_stream_gated)
        .collect())
}

struct Player {
    child: Child,
    socket: UnixStream,
    events: Receiver<Value>,
    directory: PathBuf,
}
impl Player {
    fn start() -> io::Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "tusic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let path = directory.join("mpv.sock");
        let child = Command::new("mpv")
            .args([
                "--no-config",
                "--idle=yes",
                "--no-video",
                "--no-terminal",
                "--audio-display=no",
                "--input-terminal=no",
                "--cache=yes",
                "--cache-on-disk=no",
                "--volume=70",
            ])
            .args(if cfg!(test) {
                vec!["--ao=null"]
            } else {
                vec![]
            })
            .arg(format!("--input-ipc-server={}", path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(io::Error::new(
                    e.kind(),
                    format!("Não foi possível iniciar mpv. Instale o mpv: {e}"),
                ));
            }
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        let socket = loop {
            match UnixStream::connect(&path) {
                Ok(s) => break s,
                Err(e) => {
                    if Instant::now() >= deadline || child.try_wait()?.is_some() {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = std::fs::remove_dir_all(&directory);
                        return Err(io::Error::new(e.kind(), format!("mpv não iniciou: {e}")));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        };
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let reader = socket.try_clone()?;
        let (sender, events) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut player = Self {
            child,
            socket,
            events,
            directory,
        };
        for (id, property) in [
            (1, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "volume"),
        ] {
            player.command(json!(["observe_property", id, property]))?;
        }
        Ok(player)
    }
    fn command(&mut self, command: Value) -> io::Result<()> {
        writeln!(self.socket, "{}", json!({"command": command}))
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Search,
    Results,
    Queue,
    Controls,
}
struct App {
    player: Player,
    query: String,
    pending: Option<Receiver<Result<Vec<Track>, String>>>,
    results: Vec<Track>,
    queue: Vec<Track>,
    result_state: ListState,
    queue_state: ListState,
    current: Option<Track>,
    previous: Vec<Track>,
    focus: Focus,
    selected_queue: bool,
    button: usize,
    paused: bool,
    position: f64,
    duration: f64,
    volume: f64,
    status: String,
    areas: [Rect; 5],
    buttons: Vec<Rect>,
    exit: bool,
}
impl App {
    fn new(player: Player) -> Self {
        Self {
            player,
            query: String::new(),
            pending: None,
            results: vec![],
            queue: vec![],
            result_state: ListState::default(),
            queue_state: ListState::default(),
            current: None,
            previous: vec![],
            focus: Focus::Search,
            selected_queue: false,
            button: 0,
            paused: false,
            position: 0.,
            duration: 0.,
            volume: 70.,
            status: "Digite uma busca e pressione Enter.".into(),
            areas: [Rect::default(); 5],
            buttons: vec![],
            exit: false,
        }
    }
    fn send(&mut self, command: Value) {
        if let Err(e) = self.player.command(command) {
            self.status = format!("Erro no player: {e}");
        }
    }
    fn play(&mut self, track: Track) {
        if let Some(old) = self.current.take() {
            self.previous.push(old);
        }
        self.position = 0.;
        self.duration = track.duration as f64;
        self.paused = false;
        self.status = format!("Carregando: {}", track.title);
        self.send(json!([
            "loadfile",
            format!("{API}/tracks/{}/stream?app_name=tusic", track.id),
            "replace"
        ]));
        self.send(json!(["set_property", "pause", false]));
        self.current = Some(track);
    }
    fn next(&mut self) {
        if !self.queue.is_empty() {
            let track = self.queue.remove(0);
            self.queue_state
                .select(if self.queue.is_empty() { None } else { Some(0) });
            self.play(track);
        } else {
            if let Some(old) = self.current.take() {
                self.previous.push(old);
            }
            self.send(json!(["stop"]));
            self.position = 0.;
            self.duration = 0.;
            self.status = "Fila encerrada.".into();
        }
    }
    fn activate(&mut self, action: usize) {
        match action {
            0 => {
                let track = if self.focus == Focus::Queue
                    || (self.focus == Focus::Controls && self.selected_queue)
                {
                    if let Some(i) = self.queue_state.selected()
                        && i < self.queue.len()
                    {
                        let track = self.queue.remove(i);
                        self.queue_state.select(if self.queue.is_empty() {
                            None
                        } else {
                            Some(i.min(self.queue.len() - 1))
                        });
                        Some(track)
                    } else {
                        None
                    }
                } else {
                    self.result_state
                        .selected()
                        .and_then(|i| self.results.get(i))
                        .cloned()
                };
                if let Some(track) = track {
                    self.play(track);
                }
            }
            1 => {
                if let Some(track) = self
                    .result_state
                    .selected()
                    .and_then(|i| self.results.get(i))
                    .cloned()
                {
                    self.queue.push(track);
                    if self.queue_state.selected().is_none() {
                        self.queue_state.select(Some(0));
                    }
                    self.status = "Música adicionada à fila.".into();
                }
            }
            2 => {
                if let Some(track) = self.previous.pop() {
                    self.play(track);
                }
            }
            3 => self.send(json!(["cycle", "pause"])),
            4 => self.next(),
            5 | 6 => self.send(json!([
                "set_property",
                "volume",
                (self.volume + if action == 5 { -5. } else { 5. }).clamp(0., 100.)
            ])),
            7 => self.exit = true,
            _ => {}
        }
    }
    fn begin_search(&mut self) {
        if self.query.trim().is_empty() {
            self.status = "Digite o nome de uma música ou artista.".into();
            return;
        }
        let query = self.query.trim().to_string();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(search(&query));
        });
        self.pending = Some(receiver);
        self.status = "Buscando no Audius…".into();
    }
    fn update(&mut self) {
        if let Some(result) = self.pending.as_ref().and_then(|r| r.try_recv().ok()) {
            self.pending = None;
            match result {
                Ok(tracks) => {
                    self.status = format!("{} faixas públicas encontradas.", tracks.len());
                    self.results = tracks;
                    self.result_state.select(if self.results.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    self.focus = Focus::Results;
                }
                Err(e) => self.status = e,
            }
        }
        while let Ok(value) = self.player.events.try_recv() {
            match value["event"].as_str() {
                Some("property-change") => match value["name"].as_str() {
                    Some("time-pos") => self.position = value["data"].as_f64().unwrap_or(0.),
                    Some("duration") => {
                        if let Some(d) = value["data"].as_f64() {
                            self.duration = d;
                        }
                    }
                    Some("pause") => self.paused = value["data"].as_bool().unwrap_or(false),
                    Some("volume") => self.volume = value["data"].as_f64().unwrap_or(self.volume),
                    _ => {}
                },
                Some("file-loaded") => self.status = "Reproduzindo via streaming.".into(),
                Some("end-file") => match value["reason"].as_str() {
                    Some("eof") => self.next(),
                    Some("error") => {
                        self.current = None;
                        self.position = 0.;
                        self.duration = 0.;
                        self.status = format!(
                            "Não foi possível reproduzir: {}. Escolha outra faixa ou tente novamente.",
                            value["file_error"].as_str().unwrap_or("erro de streaming")
                        );
                    }
                    _ => {}
                },
                _ => {
                    if let Some(error) = value["error"].as_str()
                        && error != "success"
                    {
                        self.status = format!("mpv: {error}");
                    }
                }
            }
        }
    }
    fn move_selection(&mut self, down: bool) {
        let (state, len) = match self.focus {
            Focus::Results => (&mut self.result_state, self.results.len()),
            Focus::Queue => (&mut self.queue_state, self.queue.len()),
            _ => return,
        };
        if len > 0 {
            let index = state.selected().unwrap_or(0);
            state.select(Some(if down {
                (index + 1).min(len - 1)
            } else {
                index.saturating_sub(1)
            }));
        }
    }
    fn handle(&mut self, event: Event) {
        if self.focus == Focus::Queue {
            self.selected_queue = true;
        } else if self.focus == Focus::Results {
            self.selected_queue = false;
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.exit = true;
                    return;
                }
                match key.code {
                    KeyCode::Esc => self.focus = Focus::Results,
                    KeyCode::Tab => {
                        self.focus = match self.focus {
                            Focus::Search => Focus::Results,
                            Focus::Results => Focus::Queue,
                            Focus::Queue => Focus::Controls,
                            Focus::Controls => Focus::Search,
                        }
                    }
                    KeyCode::BackTab => {
                        self.focus = match self.focus {
                            Focus::Search => Focus::Controls,
                            Focus::Results => Focus::Search,
                            Focus::Queue => Focus::Results,
                            Focus::Controls => Focus::Queue,
                        }
                    }
                    KeyCode::Enter => match self.focus {
                        Focus::Search => self.begin_search(),
                        Focus::Controls => self.activate(self.button),
                        _ => self.activate(0),
                    },
                    KeyCode::Char(c)
                        if self.focus == Focus::Search
                            && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        self.query.push(c)
                    }
                    KeyCode::Backspace if self.focus == Focus::Search => {
                        self.query.pop();
                    }
                    KeyCode::Up => self.move_selection(false),
                    KeyCode::Down => self.move_selection(true),
                    KeyCode::Left if self.focus == Focus::Controls => {
                        self.button = self.button.saturating_sub(1)
                    }
                    KeyCode::Right if self.focus == Focus::Controls => {
                        self.button = (self.button + 1).min(7)
                    }
                    KeyCode::Char(' ') => self.activate(3),
                    KeyCode::Char('a') => self.activate(1),
                    KeyCode::Char('+') => self.activate(6),
                    KeyCode::Char('-') => self.activate(5),
                    KeyCode::Delete if self.focus == Focus::Queue => {
                        if let Some(i) = self.queue_state.selected()
                            && i < self.queue.len()
                        {
                            self.queue.remove(i);
                            self.queue_state.select(if self.queue.is_empty() {
                                None
                            } else {
                                Some(i.min(self.queue.len() - 1))
                            });
                        }
                    }
                    KeyCode::Char('q') => self.exit = true,
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                let point = ratatui::layout::Position::new(mouse.column, mouse.row);
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if self.areas[0].contains(point) {
                        self.focus = Focus::Search;
                    }
                    for (area_index, focus) in [(1, Focus::Results), (2, Focus::Queue)] {
                        if self.areas[area_index].contains(point) {
                            self.focus = focus;
                            self.selected_queue = focus == Focus::Queue;
                            let state = if focus == Focus::Results {
                                &mut self.result_state
                            } else {
                                &mut self.queue_state
                            };
                            let len = if focus == Focus::Results {
                                self.results.len()
                            } else {
                                self.queue.len()
                            };
                            let area = self.areas[area_index];
                            if mouse.row > area.y && mouse.row < area.bottom().saturating_sub(1) {
                                let index = state.offset() + usize::from(mouse.row - area.y - 1);
                                if index < len {
                                    state.select(Some(index));
                                }
                            }
                        }
                    }
                    if let Some(i) = self.buttons.iter().position(|r| r.contains(point)) {
                        self.activate(i);
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    if self.areas[1].contains(point) {
                        self.focus = Focus::Results;
                    } else if self.areas[2].contains(point) {
                        self.focus = Focus::Queue;
                    } else {
                        return;
                    }
                    self.move_selection(mouse.kind == MouseEventKind::ScrollDown);
                }
            }
            _ => {}
        }
    }
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 70 || area.height < 16 {
            self.areas = [Rect::default(); 5];
            self.buttons.clear();
            frame.render_widget(
                Paragraph::new("Aumente o terminal para pelo menos 70 × 16. Ctrl+C para sair."),
                area,
            );
            return;
        }
        let vertical = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
        let columns = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(vertical[1]);
        self.areas = [
            vertical[0],
            columns[0],
            columns[1],
            vertical[2],
            vertical[3],
        ];
        let border = |title: &'static str, active: bool| {
            Block::bordered()
                .title(title)
                .border_style(Style::default().fg(if active {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
        };
        frame.render_widget(
            Paragraph::new(self.query.as_str()).block(border(
                " Tusic · Audius · Busca [Enter] ",
                self.focus == Focus::Search,
            )),
            vertical[0],
        );
        if self.focus == Focus::Search {
            let width = ratatui::text::Line::from(self.query.as_str())
                .width()
                .min(u16::MAX as usize) as u16;
            frame.set_cursor_position((
                vertical[0].x + 1 + width.min(vertical[0].width.saturating_sub(3)),
                vertical[0].y + 1,
            ));
        }
        let items: Vec<_> = self
            .results
            .iter()
            .map(|t| ListItem::new(t.label()))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(border(" Resultados ", self.focus == Focus::Results))
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                .highlight_symbol("› "),
            columns[0],
            &mut self.result_state,
        );
        let items: Vec<_> = self
            .queue
            .iter()
            .map(|t| ListItem::new(t.label()))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(border(" Fila [Del remove] ", self.focus == Focus::Queue))
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                .highlight_symbol("› "),
            columns[1],
            &mut self.queue_state,
        );
        let title = self
            .current
            .as_ref()
            .map(|t| {
                format!(
                    " {} · {} ",
                    if self.paused { "Pausado" } else { "Tocando" },
                    t.label()
                )
            })
            .unwrap_or_else(|| " Nenhuma música tocando ".into());
        frame.render_widget(
            Gauge::default()
                .block(Block::bordered().title(title))
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(if self.duration > 0. {
                    (self.position / self.duration).clamp(0., 1.)
                } else {
                    0.
                })
                .label(format!(
                    "{} / {} · Volume {:.0}%",
                    clock(self.position),
                    clock(self.duration),
                    self.volume
                )),
            vertical[2],
        );
        self.buttons = Layout::horizontal([Constraint::Ratio(1, 8); 8])
            .split(vertical[3])
            .to_vec();
        for (i, label) in [
            "Tocar",
            "+ Fila",
            "Anterior",
            if self.paused { "Retomar" } else { "Pausar" },
            "Próxima",
            "Vol −",
            "Vol +",
            "Sair",
        ]
        .iter()
        .enumerate()
        {
            frame.render_widget(
                Paragraph::new(*label).centered().block(border(
                    "",
                    self.focus == Focus::Controls && self.button == i,
                )),
                self.buttons[i],
            );
        }
        frame.render_widget(
            Paragraph::new(format!(
                "{}\nTab: foco · ↑↓: selecionar · Espaço: pausa · a: fila · +/−: volume · q: sair",
                self.status
            )),
            vertical[4],
        );
    }
    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            self.update();
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))? {
                self.handle(event::read()?);
            }
            if let Some(status) = self.player.child.try_wait()? {
                return Err(io::Error::other(format!("mpv encerrou: {status}")));
            }
        }
        Ok(())
    }
}
fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
fn main() -> io::Result<()> {
    let player = Player::start()?;
    let mut terminal = ratatui::init();
    let result = execute!(io::stdout(), EnableMouseCapture)
        .and_then(|_| App::new(player).run(&mut terminal));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_mouse_and_controls_render() {
        use crossterm::event::{KeyEvent, MouseEvent};
        let mut app = App::new(Player::start().unwrap());
        app.query = "ambient".into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        app.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(app.focus == Focus::Results);
        let button = app.buttons[7];
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.x + 1,
            row: button.y + 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.exit);
    }
    #[test]
    #[ignore = "requires internet access to Audius"]
    fn live_search_and_stream() {
        let tracks = search("ambient").unwrap();
        assert!(!tracks.is_empty());
        let mut app = App::new(Player::start().unwrap());
        app.play(tracks[0].clone());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            app.update();
            if app.position > 0. {
                return;
            }
            assert!(app.current.is_some(), "{}", app.status);
            thread::sleep(Duration::from_millis(100));
        }
        panic!("stream did not advance: {}", app.status);
    }
    #[test]
    fn queue_advances_and_stops_without_repeating() {
        let player = Player::start().expect("mpv required for integration test");
        let mut app = App::new(player);
        let track = Track {
            id: "test".into(),
            title: "Test".into(),
            user: Artist {
                name: "Artist".into(),
            },
            duration: 120,
            is_stream_gated: false,
            is_streamable: true,
            is_available: true,
        };
        app.current = Some(track.clone());
        app.queue.push(track);
        app.next();
        assert!(app.queue.is_empty());
        assert!(app.current.is_some());
        app.next();
        assert!(app.current.is_none());
        assert_eq!(app.previous.len(), 2);
    }
    #[test]
    fn parses_public_and_restricted_tracks() {
        let response: SearchResponse = serde_json::from_value(json!({"data": [
            {"id":"1", "title":"Public", "user":{"name":"Artist"}, "duration":10, "is_streamable":true, "is_available":true},
            {"id":"2", "title":"Paid", "user":{"name":"Artist"}, "duration":10, "is_streamable":true, "is_available":true, "is_stream_gated":true}
        ]})).unwrap();
        let public: Vec<_> = response
            .data
            .into_iter()
            .filter(|t| t.is_streamable && t.is_available && !t.is_stream_gated)
            .collect();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].id, "1");
        assert_eq!(clock(125.), "2:05");
    }
}
