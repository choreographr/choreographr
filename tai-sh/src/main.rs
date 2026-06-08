use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use std::{collections::HashSet, io, sync::Arc, time::Duration};
use tai_proto::{read_message, socket_path, write_message, ClientMessage, DaemonMessage};
use tai_sh::{
    ImageAssembler, RenderedImage, ShellCommand, build_picker, build_rendered_image,
    channel_closed, parse_input_line,
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::{Mutex, mpsc},
};

struct App {
    input: String,
    next_request_id: u32,
    active: HashSet<u32>,
    history: Vec<HistoryItem>,
    scroll: usize,
    should_quit: bool,
}

enum HistoryItem {
    Text(String),
    Image(RenderedImage),
}

impl App {
    fn new(socket_path: String, picker_protocol: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            active: HashSet::new(),
            history: vec![
                HistoryItem::Text(format!("Connected to tai-daemon at {socket_path}")),
                HistoryItem::Text(format!("image protocol: {picker_protocol}")),
            ],
            scroll: 0,
            should_quit: false,
        }
    }

    fn push_text(&mut self, line: impl Into<String>) {
        self.history.push(HistoryItem::Text(line.into()));
        self.scroll = 0;
        if self.history.len() > 500 {
            let excess = self.history.len() - 500;
            self.history.drain(0..excess);
        }
    }

    fn push_image(&mut self, image: RenderedImage) {
        self.history.push(HistoryItem::Image(image));
        self.scroll = 0;
    }

    fn drop_request(&mut self, request_id: u32) {
        self.active.remove(&request_id);
    }
}

enum UiEvent {
    Daemon(DaemonMessage),
    ReaderClosed,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket_path = socket_path();
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();
    let (client_tx, mut client_rx) = mpsc::channel::<ClientMessage>(128);
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(128);
    let active = Arc::new(Mutex::new(HashSet::<u32>::new()));

    let picker = build_picker();
    let picker_protocol = format!("{:?}", picker.protocol_type());

    let writer_task = tokio::spawn(async move {
        while let Some(message) = client_rx.recv().await {
            write_message(&mut writer, &message).await?;
        }
        writer.shutdown().await
    });

    let reader_ui_tx = ui_tx.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_message::<_, DaemonMessage>(&mut reader).await {
                Ok(message) => {
                    if reader_ui_tx.send(UiEvent::Daemon(message)).await.is_err() {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    let _ = reader_ui_tx.send(UiEvent::ReaderClosed).await;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok::<(), io::Error>(())
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(socket_path.clone(), picker_protocol);
    let mut assembler = ImageAssembler::new();

    let result = run_ui_loop(&mut terminal, &mut app, &picker, &mut assembler, &client_tx, &mut ui_rx, &active).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    drop(client_tx);
    writer_task.await.map_err(io::Error::other)??;
    match reader_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(io::Error::other(error)),
    }

    result
}

async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    assembler: &mut ImageAssembler,
    client_tx: &mpsc::Sender<ClientMessage>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    active: &Arc<Mutex<HashSet<u32>>>,
) -> io::Result<()> {
    while !app.should_quit {
        while let Ok(event) = event::poll(Duration::from_millis(0)) {
            if !event {
                break;
            }
            handle_terminal_event(event::read()?, app, client_tx).await?;
        }

        while let Ok(message) = ui_rx.try_recv() {
            match message {
                UiEvent::Daemon(message) => {
                    handle_daemon_message(message, app, picker, assembler, active).await?;
                }
                UiEvent::ReaderClosed => {
                    app.push_text("daemon connection closed");
                    app.should_quit = true;
                }
            }
        }

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(16))? {
            handle_terminal_event(event::read()?, app, client_tx).await?;
        }
    }

    Ok(())
}

async fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &mpsc::Sender<ClientMessage>,
) -> io::Result<()> {
    if let Event::Key(key) = event {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('q') if app.input.is_empty() => app.should_quit = true,
            KeyCode::Esc => app.should_quit = true,
            KeyCode::Enter => {
                let line = app.input.trim().to_string();
                app.input.clear();
                match parse_input_line(&line, &mut app.next_request_id) {
                    ShellCommand::Empty => {}
                    ShellCommand::InvalidCancel(value) => app.push_text(format!("invalid request id: {value}")),
                    ShellCommand::Send(message) => {
                        if let ClientMessage::RunInput { request_id, input } = &message {
                            app.active.insert(*request_id);
                            app.push_text(format!("> {}", String::from_utf8_lossy(input)));
                        }
                        client_tx.send(message).await.map_err(channel_closed)?;
                    }
                }
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => {
                app.input.push(c);
            }
            KeyCode::PageUp => {
                app.scroll = app.scroll.saturating_add(3);
            }
            KeyCode::PageDown => {
                app.scroll = app.scroll.saturating_sub(3);
            }
            _ => {}
        }
    }
    Ok(())
}

async fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    assembler: &mut ImageAssembler,
    active: &Arc<Mutex<HashSet<u32>>>,
) -> io::Result<()> {
    match message {
        DaemonMessage::Started { request_id } => {
            app.push_text(format!("[{request_id}] started"));
        }
        DaemonMessage::OutputChunk { request_id, data, .. } => {
            for line in String::from_utf8_lossy(&data).lines() {
                app.push_text(format!("[{request_id}] {line}"));
            }
        }
        DaemonMessage::ImageStart { request_id, metadata } => {
            assembler.start(request_id, metadata)?;
        }
        DaemonMessage::ImageChunk { request_id, image_id, data } => {
            assembler.push_chunk(request_id, image_id, &data)?;
        }
        DaemonMessage::ImageEnd { request_id, image_id } => {
            let (metadata, data) = assembler.finish(request_id, image_id)?;
            let rendered = build_rendered_image(picker, metadata, data)?;
            app.push_image(rendered);
        }
        DaemonMessage::Done { request_id } => {
            app.push_text(format!("[{request_id}] done"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Failed { request_id, error } => {
            app.push_text(format!("[{request_id}] failed: {error}"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Cancelled { request_id } => {
            app.push_text(format!("[{request_id}] cancelled"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Pong => app.push_text("[daemon] pong".to_string()),
    }
    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    let history_block = Block::default().borders(Borders::ALL).title("history");
    let history_area = history_block.inner(chunks[0]);
    frame.render_widget(history_block, chunks[0]);
    render_history(frame, history_area, app);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);

    let cursor_x = chunks[1].x.saturating_add(1 + app.input.chars().count() as u16);
    let cursor_y = chunks[1].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let skip = app.scroll;
    let mut seen = 0usize;

    for item in app.history.iter_mut().rev() {
        if seen < skip {
            seen += 1;
            continue;
        }
        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) => {
                let wrapped = history_text_height(text).max(1);
                if wrapped > rows_remaining {
                    break;
                }
                y = y.saturating_sub(wrapped as u16);
                let rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: wrapped as u16,
                };
                frame.render_widget(Paragraph::new(Line::from(text.as_str())).wrap(Wrap { trim: false }), rect);
                rows_remaining -= wrapped;
            }
            HistoryItem::Image(image) => {
                let height = image_history_height(rows_remaining) as u16;
                if height == 0 {
                    break;
                }
                y = y.saturating_sub(height);
                let block = Block::default().title(format!(
                    "image {} ({} {}x{})",
                    image.metadata.image_id,
                    image.metadata.mime_type,
                    image.metadata.width,
                    image.metadata.height
                ));
                let rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                };
                let inner = block.inner(rect);
                frame.render_widget(block, rect);
                frame.render_stateful_widget(StatefulImage::default(), inner, &mut image.protocol);
                rows_remaining = rows_remaining.saturating_sub(height as usize);
            }
        }
    }
}

fn history_text_height(text: &str) -> usize {
    text.lines().count().max(1)
}

fn image_history_height(rows_remaining: usize) -> usize {
    rows_remaining.min(12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn app_push_text_trims_history_to_limit() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Halfblocks".to_string());
        for index in 0..600 {
            app.push_text(format!("line {index}"));
        }
        assert_eq!(app.history.len(), 500);
        match &app.history[0] {
            HistoryItem::Text(text) => assert!(text.contains("line 100")),
            HistoryItem::Image(_) => panic!("expected text history item"),
        }
    }

    #[test]
    fn drop_request_removes_active_request() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.active.insert(42);
        app.drop_request(42);
        assert!(!app.active.contains(&42));
    }

    #[test]
    fn history_text_height_counts_non_empty_lines() {
        assert_eq!(history_text_height("hello"), 1);
        assert_eq!(history_text_height("a\nb\n"), 2);
        assert_eq!(history_text_height(""), 1);
    }

    #[test]
    fn image_history_height_caps_to_twelve_rows() {
        assert_eq!(image_history_height(0), 0);
        assert_eq!(image_history_height(4), 4);
        assert_eq!(image_history_height(20), 12);
    }

    #[tokio::test]
    async fn terminal_event_appends_characters() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        let (tx, mut rx) = mpsc::channel(1);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle key");
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle key");

        assert_eq!(app.input, "hi");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn terminal_event_submits_run_input() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.input = "hello".to_string();
        let (tx, mut rx) = mpsc::channel(1);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle enter");

        assert!(app.input.is_empty());
        let message = rx.recv().await.expect("sent message");
        assert_eq!(
            message,
            ClientMessage::RunInput {
                request_id: 1,
                input: b"hello".to_vec(),
            }
        );
    }

    #[tokio::test]
    async fn terminal_event_quits_only_when_input_empty() {
        let (tx, _rx) = mpsc::channel(1);

        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle q");
        assert!(app.should_quit);

        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.input = "q".to_string();
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle q");
        assert!(!app.should_quit);
        assert_eq!(app.input, "qq");
    }
}
