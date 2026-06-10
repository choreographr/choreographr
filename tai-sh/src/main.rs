use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use std::{collections::{HashMap, HashSet}, io, sync::Arc, time::Duration};
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
    in_progress: HashMap<u32, usize>,
    scroll: usize,
    should_quit: bool,
}

enum HistoryItem {
    Text(String),
    StreamingText(String),
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
            in_progress: HashMap::new(),
            scroll: 0,
            should_quit: false,
        }
    }

    fn push_text(&mut self, line: impl Into<String>) {
        self.history.push(HistoryItem::Text(line.into()));
        self.scroll = 0;
        self.trim_history();
    }

    fn push_image(&mut self, image: RenderedImage) {
        self.history.push(HistoryItem::Image(image));
        self.scroll = 0;
        self.trim_history();
    }

    fn begin_stream(&mut self, request_id: u32) {
        if self.in_progress.contains_key(&request_id) {
            return;
        }
        let index = self.history.len();
        self.history
            .push(HistoryItem::StreamingText(format!("[{request_id}] ")));
        self.in_progress.insert(request_id, index);
        self.scroll = 0;
        self.trim_history();
    }

    fn append_stream_text(&mut self, request_id: u32, chunk: &str) {
        if !self.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.in_progress.get(&request_id)
            && let Some(HistoryItem::StreamingText(text)) = self.history.get_mut(index)
        {
            text.push_str(chunk);
        }
        self.scroll = 0;
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.in_progress.remove(&request_id);
    }

    fn drop_request(&mut self, request_id: u32) {
        self.active.remove(&request_id);
        self.finalize_stream(request_id);
    }

    fn trim_history(&mut self) {
        if self.history.len() <= 500 {
            return;
        }
        let excess = self.history.len() - 500;
        self.history.drain(0..excess);
        for index in self.in_progress.values_mut() {
            *index = index.saturating_sub(excess);
        }
        self.in_progress.retain(|_, index| *index < self.history.len());
    }
}

enum UiEvent {
    Daemon(DaemonMessage),
    ReaderClosed,
    Interrupt,
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

    let signal_ui_tx = ui_tx.clone();
    let signal_task = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.map_err(io::Error::other)?;
        let _ = signal_ui_tx.send(UiEvent::Interrupt).await;
        Ok::<(), io::Error>(())
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(socket_path.clone(), picker_protocol);
    let mut assembler = ImageAssembler::new();

    let result = run_ui_loop(&mut terminal, &mut app, &picker, &mut assembler, &client_tx, &mut ui_rx, &active).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    drop(client_tx);
    writer_task.await.map_err(io::Error::other)??;
    signal_task.abort();
    match signal_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(io::Error::other(error)),
    }

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
                UiEvent::Interrupt => {
                    app.push_text("interrupt received");
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
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                    app.should_quit = true;
                }
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
        Event::Mouse(mouse) => {
            if mouse_in_history_box(mouse.column, mouse.row) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll = app.scroll.saturating_add(1);
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll = app.scroll.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
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
            app.begin_stream(request_id);
        }
        DaemonMessage::OutputChunk { request_id, data, .. } => {
            let text = String::from_utf8(data)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            app.append_stream_text(request_id, &text);
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
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] done"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Failed { request_id, error } => {
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] failed: {error}"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Cancelled { request_id } => {
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] cancelled"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Pong => app.push_text("[daemon] pong".to_string()),
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                app.push_text("[daemon] no models available".to_string());
            } else {
                app.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    app.push_text(format!("{prefix} {model}"));
                }
            }
        }
        DaemonMessage::ModelSelected { model } => {
            app.push_text(format!("[daemon] selected model: {model}"));
        }
    }
    Ok(())
}

fn mouse_in_history_box(column: u16, row: u16) -> bool {
    let Ok((width, height)) = crossterm::terminal::size() else {
        return false;
    };

    if width == 0 || height == 0 || height <= 3 {
        return false;
    }

    let input_height = 3;
    let history_height = height.saturating_sub(input_height);
    column < width && row < history_height
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    render_history(frame, chunks[0], app);

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
    let mut rows_to_skip = app.scroll;

    for item in app.history.iter_mut().rev() {
        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) | HistoryItem::StreamingText(text) => {
                let wrapped = history_text_height(text, area.width).max(1);
                if rows_to_skip >= wrapped {
                    rows_to_skip -= wrapped;
                    continue;
                }

                let visible_height = wrapped.min(rows_remaining);
                if visible_height == 0 {
                    break;
                }

                let bottom_line = wrapped.saturating_sub(rows_to_skip);
                let top_line = bottom_line.saturating_sub(visible_height);

                y = y.saturating_sub(visible_height as u16);
                let rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_height as u16,
                };
                frame.render_widget(
                    Paragraph::new(text.as_str())
                        .wrap(Wrap { trim: false })
                        .scroll((top_line as u16, 0)),
                    rect,
                );
                rows_remaining -= visible_height;
                rows_to_skip = 0;
            }
            HistoryItem::Image(image) => {
                let full_height = image_history_height(area.height as usize);
                if rows_to_skip >= full_height {
                    rows_to_skip -= full_height;
                    continue;
                }

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
                rows_to_skip = 0;
            }
        }
    }
}

fn history_text_height(text: &str, width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if text.is_empty() {
        return 1;
    }

    text.split('\n')
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
        .max(1)
}

fn wrapped_line_height(line: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }

    let line_width = line.chars().count();
    if line_width == 0 {
        1
    } else {
        line_width.div_ceil(width)
    }
}

fn image_history_height(rows_remaining: usize) -> usize {
    rows_remaining.min(12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};

    #[test]
    fn app_push_text_trims_history_to_limit() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Halfblocks".to_string());
        for index in 0..600 {
            app.push_text(format!("line {index}"));
        }
        assert_eq!(app.history.len(), 500);
        match &app.history[0] {
            HistoryItem::Text(text) => assert!(text.contains("line 100")),
            HistoryItem::StreamingText(_) | HistoryItem::Image(_) => panic!("expected text history item"),
        }
    }

    #[test]
    fn drop_request_removes_active_request() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.active.insert(42);
        app.begin_stream(42);
        app.drop_request(42);
        assert!(!app.active.contains(&42));
        assert!(!app.in_progress.contains_key(&42));
    }

    #[test]
    fn append_stream_text_updates_mutable_history_entry() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.begin_stream(7);
        app.append_stream_text(7, "hello");
        app.append_stream_text(7, " world");

        let index = app.in_progress[&7];
        match &app.history[index] {
            HistoryItem::StreamingText(text) => {
                assert_eq!(text, "[7] hello world");
            }
            _ => panic!("expected streaming text item"),
        }
    }

    #[test]
    fn history_text_height_accounts_for_wrapping_and_blank_lines() {
        assert_eq!(history_text_height("hello", 10), 1);
        assert_eq!(history_text_height("hello world", 5), 3);
        assert_eq!(history_text_height("a\nb\n", 10), 3);
        assert_eq!(history_text_height("", 10), 1);
        assert_eq!(history_text_height("\n", 10), 2);
    }

    #[test]
    fn oversized_history_item_keeps_visible_tail() {
        let wrapped = history_text_height("123456789", 3);
        assert_eq!(wrapped, 3);

        let rows_remaining = 2;
        let rows_to_skip = 0;
        let bottom_line = wrapped.saturating_sub(rows_to_skip);
        let top_line = bottom_line.saturating_sub(rows_remaining);

        assert_eq!(top_line, 1);
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

    #[tokio::test]
    async fn terminal_event_ctrl_c_quits() {
        let (tx, _rx) = mpsc::channel(1);
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut app,
            &tx,
        )
        .await
        .expect("handle ctrl+c");

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn mouse_scroll_outside_history_box_does_not_change_scroll() {
        let (tx, _rx) = mpsc::channel(1);
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.scroll = 5;

        let (_, height) = crossterm::terminal::size().expect("terminal size");
        let row = height.saturating_sub(1);
        handle_terminal_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            &mut app,
            &tx,
        )
        .await
        .expect("handle mouse");

        assert_eq!(app.scroll, 5);
    }
}
