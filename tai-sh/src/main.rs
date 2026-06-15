use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::StatefulImage;
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
    time::Duration,
};
use tai_proto::{
    ClientMessage, DaemonMessage, OutputStream, SessionMessage, read_message, socket_path,
    write_message,
};
use tai_sh::{
    ImageAssembler, MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline,
    RenderedImage, ShellCommand, StreamingText, build_picker, build_rendered_image, channel_closed,
    parse_input_line,
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::{Mutex, mpsc},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

struct App {
    input: String,
    next_request_id: u32,
    active: HashSet<u32>,
    history: Vec<HistoryItem>,
    in_progress: HashMap<u32, usize>,
    history_scroll: HistoryScrollState,
    history_viewport: HistoryViewport,
    should_quit: bool,
}

#[derive(Clone, Copy)]
struct HistoryViewport {
    width: u16,
    height: u16,
}

#[derive(Clone, Copy)]
struct HistoryScrollState {
    scroll: usize,
    scroll_compensation: usize,
    follow_output: bool,
}

enum HistoryItem {
    Text(String),
    SessionMessage(SessionMessage),
    StreamingText(StreamingTextItem),
    Image(RenderedImage),
}

type StreamingTextItem = StreamingText;

impl HistoryViewport {
    fn new() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    fn update(&mut self, area: Rect) {
        self.width = area.width.max(1);
        self.height = area.height;
    }

    fn item_height(&self, item: &HistoryItem) -> usize {
        match item {
            HistoryItem::Text(text) => history_text_height(text, self.width).max(1),
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::StreamingText(text) => {
                let lines = streaming_text_lines(text, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::Image(_) => image_block_height(self.height as usize),
        }
    }
}

impl HistoryScrollState {
    fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
            follow_output: true,
        }
    }

    #[cfg(test)]
    fn scroll(&self) -> usize {
        self.scroll
    }

    #[cfg(test)]
    fn scroll_compensation(&self) -> usize {
        self.scroll_compensation
    }

    fn follow_output(&self) -> bool {
        self.follow_output
    }

    fn unclamped_effective_scroll(&self) -> usize {
        self.scroll.saturating_add(self.scroll_compensation)
    }

    fn clamp(&mut self, max_scroll: usize) {
        let effective = self.unclamped_effective_scroll();
        if effective <= max_scroll {
            return;
        }

        let overflow = effective - max_scroll;
        let compensation_reduction = self.scroll_compensation.min(overflow);
        self.scroll_compensation -= compensation_reduction;
        let remaining = overflow - compensation_reduction;
        self.scroll = self.scroll.saturating_sub(remaining);
        if self.scroll == 0 && self.scroll_compensation == 0 {
            self.follow_output = true;
        }
    }

    fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    fn preserve_for_growth(&mut self, old_height: usize, new_height: usize, max_scroll: usize) {
        if !self.follow_output && new_height > old_height {
            self.scroll_compensation = self
                .scroll_compensation
                .saturating_add(new_height - old_height);
            self.clamp(max_scroll);
        }
    }

    fn on_item_appended(&mut self, added_height: usize, max_scroll: usize) {
        if self.follow_output {
            self.scroll = 0;
            self.scroll_compensation = 0;
        } else {
            self.scroll_compensation = self.scroll_compensation.saturating_add(added_height);
        }
        self.clamp(max_scroll);
    }

    fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        if self.scroll > 0 {
            self.follow_output = false;
        }
        self.clamp(max_scroll);
    }

    fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
        if self.scroll == 0 && self.scroll_compensation == 0 {
            self.follow_output = true;
        }
        self.clamp(max_scroll);
    }

    fn account_for_trimmed_height(&mut self, trimmed_height: usize, max_scroll: usize) {
        self.scroll_compensation = self.scroll_compensation.saturating_sub(trimmed_height);
        self.clamp(max_scroll);
    }
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
            history_scroll: HistoryScrollState::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
        }
    }

    fn total_history_height(&self) -> usize {
        self.history
            .iter()
            .map(|item| self.history_viewport.item_height(item))
            .sum()
    }

    fn max_scroll_offset(&self) -> usize {
        let viewport_height = self.history_viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    fn clamp_scroll_state(&mut self) {
        self.history_scroll.clamp(self.max_scroll_offset());
    }

    fn effective_scroll(&self) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset())
    }

    fn preserve_scroll_for_growth(&mut self, old_height: usize, new_height: usize) {
        self.history_scroll
            .preserve_for_growth(old_height, new_height, self.max_scroll_offset());
    }

    fn push_text(&mut self, line: impl Into<String>) {
        self.push_history_item(HistoryItem::Text(line.into()));
    }

    fn push_session_message(&mut self, message: SessionMessage) {
        self.push_history_item(HistoryItem::SessionMessage(message));
    }

    fn push_image(&mut self, image: RenderedImage) {
        let item = HistoryItem::Image(image);
        self.push_history_item(item);
    }

    fn push_history_item(&mut self, item: HistoryItem) {
        let added_height = self.history_viewport.item_height(&item);
        self.history.push(item);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.trim_history();
        self.clamp_scroll_state();
    }

    fn begin_stream(&mut self, request_id: u32) {
        if self.in_progress.contains_key(&request_id) {
            return;
        }
        let index = self.history.len();
        let item = HistoryItem::StreamingText(StreamingTextItem::new(request_id));
        let added_height = self.history_viewport.item_height(&item);
        self.history.push(item);
        self.in_progress.insert(request_id, index);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.trim_history();
        self.clamp_scroll_state();
    }

    fn append_stream_text(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        if !self.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.in_progress.get(&request_id) {
            let old_height = self
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(0);
            if let Some(HistoryItem::StreamingText(text)) = self.history.get_mut(index) {
                text.append(stream, chunk);
            }
            let new_height = self
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(old_height);
            self.preserve_scroll_for_growth(old_height, new_height);
        }
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.in_progress.remove(&request_id);
    }

    fn scroll_up(&mut self, amount: usize) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset());
    }

    fn scroll_down(&mut self, amount: usize) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset());
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
        let trimmed_height = if self.history_scroll.follow_output() {
            0
        } else {
            self.history
                .iter()
                .take(excess)
                .map(|item| self.history_viewport.item_height(item))
                .sum::<usize>()
        };
        self.history.drain(0..excess);
        for index in self.in_progress.values_mut() {
            *index = index.saturating_sub(excess);
        }
        self.in_progress
            .retain(|_, index| *index < self.history.len());
        self.history_scroll
            .account_for_trimmed_height(trimmed_height, self.max_scroll_offset());
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
    client_tx
        .send(ClientMessage::ListSessions)
        .await
        .map_err(channel_closed)?;

    let result = run_ui_loop(
        &mut terminal,
        &mut app,
        &picker,
        &mut assembler,
        &client_tx,
        &mut ui_rx,
        &active,
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
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
                    handle_daemon_message(message, app, picker, assembler, active, client_tx)
                        .await?;
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
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.should_quit = true;
                }
                KeyCode::Char('q') if app.input.is_empty() => app.should_quit = true,
                KeyCode::Esc => app.should_quit = true,
                KeyCode::Enter => {
                    let line = app.input.trim().to_string();
                    app.input.clear();
                    match parse_input_line(&line, &mut app.next_request_id) {
                        ShellCommand::Empty => {}
                        ShellCommand::InvalidCancel(value) => {
                            app.push_text(format!("invalid request id: {value}"))
                        }
                        ShellCommand::Send(message) => {
                            match &message {
                                ClientMessage::RunInput { request_id, input } => {
                                    app.active.insert(*request_id);
                                    app.push_text(format!("> {}", String::from_utf8_lossy(input)));
                                }
                                ClientMessage::TestImage { request_id } => {
                                    app.active.insert(*request_id);
                                    app.push_text("> /image".to_string());
                                }
                                _ => {}
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
                    app.scroll_up(3);
                }
                KeyCode::PageDown => {
                    app.scroll_down(3);
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) => {
            if mouse_in_history_box(mouse.column, mouse.row) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll_up(1);
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll_down(1);
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
    client_tx: &mpsc::Sender<ClientMessage>,
) -> io::Result<()> {
    match message {
        DaemonMessage::SessionCreated { session_id, title } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            app.push_text(format!("[daemon] created session {session_id}: {label}"));
        }
        DaemonMessage::Sessions { sessions } => {
            if let Some(session) = sessions.first() {
                client_tx
                    .send(ClientMessage::AttachSession {
                        session_id: session.session_id,
                    })
                    .await
                    .map_err(channel_closed)?;
            } else {
                client_tx
                    .send(ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                    })
                    .await
                    .map_err(channel_closed)?;
            }
        }
        DaemonMessage::SessionAttached { session_id } => {
            app.push_text(format!("[daemon] attached session: {session_id}"));
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            app.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = selected_model {
                app.push_text(format!("[daemon] selected model: {model}"));
            }
            for message in messages {
                app.push_session_message(message);
            }
        }
        DaemonMessage::SessionFailed { operation, error } => {
            app.push_text(format!("[daemon] {operation} failed: {error}"));
        }
        DaemonMessage::SessionMessageAppended { message } => {
            app.push_session_message(message);
        }
        DaemonMessage::Started { request_id } => {
            app.begin_stream(request_id);
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            app.push_text(format!(
                "[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"
            ));
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => {
            app.push_text(format!(
                "[{request_id}] tool {tool_name}#{call_id} ok: {output}"
            ));
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            app.push_text(format!(
                "[{request_id}] tool {tool_name}#{call_id} failed: {error}"
            ));
        }
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            app.append_stream_text(request_id, stream, &text);
        }
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            assembler.start(request_id, metadata)?;
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            assembler.push_chunk(request_id, image_id, &data)?;
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
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
        DaemonMessage::ModelsFailed { error } => {
            app.push_text(format!("[daemon] models failed: {error}"));
        }
        DaemonMessage::ModelSelected { model } => {
            app.push_text(format!("[daemon] selected model: {model}"));
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            app.push_text(format!("[daemon] failed to select model {model}: {error}"));
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

    app.history_viewport.update(chunks[0]);
    app.clamp_scroll_state();
    render_history(frame, chunks[0], app);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);

    let cursor_x = chunks[1]
        .x
        .saturating_add(1 + display_width(&app.input) as u16);
    let cursor_y = chunks[1].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    for item in app.history.iter_mut().rev() {
        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) => {
                render_history_text(
                    frame,
                    area,
                    text.as_str(),
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, area.width);
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::StreamingText(text) => {
                let lines = streaming_text_lines(text, area.width);
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::Image(image) => {
                let full_height = image_block_height(area.height as usize);
                if rows_to_skip >= full_height {
                    rows_to_skip -= full_height;
                    continue;
                }

                let height = image_block_height(rows_remaining) as u16;
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

fn render_history_text(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = history_text_height(text, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    let visible_height = wrapped.min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}

fn render_history_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = lines_height(&lines, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    let visible_height = wrapped.min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}

fn history_text_height(text: &str, width: u16) -> usize {
    let lines = plain_text_lines(text, Style::default());
    lines_height(&lines, width)
}

fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if lines.len() <= 1 && lines.iter().all(|line| line.to_string().is_empty()) {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(&line.to_string(), width))
        .sum::<usize>()
        .max(1)
}

fn plain_text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        vec![Line::from(Span::styled(String::new(), style))]
    } else {
        text.split('\n')
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect()
    }
}

fn session_message_lines(message: &SessionMessage, width: u16) -> Vec<Line<'static>> {
    match message {
        SessionMessage::SystemText { content } => labeled_plain_text_lines(
            "system",
            content,
            Style::default().add_modifier(Modifier::DIM),
        ),
        SessionMessage::UserText { content } => {
            labeled_plain_text_lines("user", content, Style::default())
        }
        SessionMessage::AssistantText { content } => {
            let body = markdown_lines(content, Style::default(), width);
            prefixed_lines(
                "assistant",
                body,
                Style::default().add_modifier(Modifier::BOLD),
            )
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning_content,
            reasoning,
            reasoning_text,
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("tool-call", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": "),
                Span::raw(
                    tool_calls
                        .iter()
                        .map(|call| format!("{}({})", call.name, call.arguments_json))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ])];
            if let Some(reasoning_text) = reasoning_content
                .as_deref()
                .or(reasoning.as_deref())
                .or(reasoning_text.as_deref())
                .filter(|value| !value.trim().is_empty())
            {
                append_section(
                    &mut lines,
                    "reasoning",
                    plain_text_lines(
                        reasoning_text,
                        Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
                    ),
                );
            }
            if let Some(content) = content.as_deref().filter(|value| !value.trim().is_empty()) {
                append_section(
                    &mut lines,
                    "content",
                    markdown_lines(content, Style::default(), width),
                );
            }
            lines
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let label = if *is_error {
                "tool error"
            } else {
                "tool result"
            };
            let style = if *is_error {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            prefixed_lines(
                label,
                plain_text_lines(&format!("{name}: {content}"), style),
                style,
            )
        }
    }
}

fn streaming_text_lines(text: &StreamingTextItem, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!("[{}]", text.request_id))];

    if !text.reasoning.is_empty() {
        append_section(
            &mut lines,
            "reasoning",
            plain_text_lines(
                &text.reasoning,
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            ),
        );
    }

    if !text.answer.is_empty() {
        append_section(
            &mut lines,
            "answer",
            markdown_lines(&text.answer, Style::default(), width),
        );
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(Line::from(""));
    }

    lines
}

fn labeled_plain_text_lines(label: &'static str, text: &str, style: Style) -> Vec<Line<'static>> {
    prefixed_lines(label, plain_text_lines(text, style), style)
}

fn prefixed_lines(
    label: &'static str,
    body: Vec<Line<'static>>,
    style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_section_with_style(&mut lines, label, body, style);
    lines
}

fn append_section(lines: &mut Vec<Line<'static>>, label: &'static str, body: Vec<Line<'static>>) {
    append_section_with_style(
        lines,
        label,
        body,
        Style::default().add_modifier(Modifier::BOLD),
    );
}

fn append_section_with_style(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    body: Vec<Line<'static>>,
    label_style: Style,
) {
    let mut body_iter = body.into_iter();
    if let Some(first) = body_iter.next() {
        let mut spans = vec![Span::styled(format!("{label}: "), label_style)];
        spans.extend(first.spans);
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(Span::styled(format!("{label}:"), label_style)));
    }

    lines.extend(body_iter);
}

fn markdown_lines(markdown: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, style, 0, width as usize);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    while matches!(lines.last(), Some(line) if line.spans.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    style: Style,
    indent: usize,
    width: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        render_markdown_block(block, lines, style, indent, width);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    style: Style,
    indent: usize,
    width: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => {
            lines.extend(inlines_to_lines(content, style, indent, None));
        }
        MarkdownBlock::Heading { level, content } => {
            let heading_style = style.add_modifier(Modifier::BOLD);
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            lines.extend(inlines_to_lines(content, heading_style, indent, prefix));
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(
                indent,
                vec![Span::styled(header, style.add_modifier(Modifier::DIM))],
            ));
            for line in code.split('\n') {
                lines.push(indented_line(
                    indent,
                    vec![Span::styled(
                        line.to_string(),
                        style.add_modifier(Modifier::DIM),
                    )],
                ));
            }
            lines.push(indented_line(
                indent,
                vec![Span::styled("```", style.add_modifier(Modifier::DIM))],
            ));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            render_markdown_blocks(
                blocks,
                &mut quoted,
                style.add_modifier(Modifier::ITALIC),
                0,
                width,
            );
            for line in quoted {
                let mut spans = vec![Span::styled(
                    "> ",
                    style.add_modifier(Modifier::DIM | Modifier::ITALIC),
                )];
                spans.extend(line.spans);
                lines.push(indented_line(indent, spans));
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", start + index)
                } else {
                    "• ".to_string()
                };
                let continuation_indent = indent + display_width(&marker);
                let mut rendered = Vec::new();
                render_markdown_blocks(item, &mut rendered, style, 0, width);
                let mut rendered_iter = rendered.into_iter();
                if let Some(first) = rendered_iter.next() {
                    let mut spans =
                        vec![Span::raw(" ".repeat(indent)), Span::styled(marker, style)];
                    spans.extend(first.spans);
                    lines.push(Line::from(spans));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(marker, style),
                    ]));
                }
                for line in rendered_iter {
                    let mut spans = vec![Span::raw(" ".repeat(continuation_indent))];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => lines.extend(render_table_lines(
            alignments, header, rows, style, indent, width,
        )),
        MarkdownBlock::Rule => lines.push(indented_line(
            indent,
            vec![Span::styled("---", style.add_modifier(Modifier::DIM))],
        )),
    }
}

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    style: Style,
    indent: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return vec![Line::from("")];
    }

    let mut table_rows = Vec::with_capacity(rows.len() + 1);
    table_rows.push(normalize_table_row(header, column_count));
    table_rows.extend(
        rows.iter()
            .map(|row| normalize_table_row(row, column_count)),
    );

    let mut widths = vec![3usize; column_count];
    for row in &table_rows {
        for (index, cell) in row.iter().enumerate() {
            for line in cell.lines() {
                widths[index] = widths[index].max(display_width(line));
            }
        }
    }

    let border_width = column_count * 3 + 1;
    let available = width
        .saturating_sub(indent)
        .max(border_width + column_count);
    let content_budget = available.saturating_sub(border_width).max(column_count);
    shrink_column_widths(&mut widths, content_budget);

    let mut lines = Vec::new();
    let header_alignment = normalized_alignments(alignments, column_count);
    lines.push(table_border_line('┌', '┬', '┐', &widths, style, indent));
    lines.extend(render_table_row_wrapped(
        &table_rows[0],
        &widths,
        &header_alignment,
        style.add_modifier(Modifier::BOLD),
        indent,
    ));
    lines.push(table_separator_line(
        &widths,
        &header_alignment,
        style,
        indent,
    ));
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        lines.extend(render_table_row_wrapped(
            row,
            &widths,
            &header_alignment,
            style,
            indent,
        ));
        if index < table_rows.len() - 1 {
            lines.push(table_border_line('├', '┼', '┤', &widths, style, indent));
        }
    }
    lines.push(table_border_line('└', '┴', '┘', &widths, style, indent));
    lines
}

fn normalized_alignments(
    alignments: &[MarkdownAlignment],
    column_count: usize,
) -> Vec<MarkdownAlignment> {
    (0..column_count)
        .map(|index| {
            alignments
                .get(index)
                .copied()
                .unwrap_or(MarkdownAlignment::None)
        })
        .collect()
}

fn normalize_table_row(row: &[Vec<MarkdownInline>], column_count: usize) -> Vec<String> {
    (0..column_count)
        .map(|index| {
            row.get(index)
                .map(|cell| inline_plain_text(cell))
                .unwrap_or_default()
        })
        .collect()
}

fn shrink_column_widths(widths: &mut [usize], budget: usize) {
    let min_width = 3usize;
    while widths.iter().sum::<usize>() > budget {
        if let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > min_width)
            .max_by_key(|(_, width)| **width)
        {
            widths[index] -= 1;
        } else {
            break;
        }
    }
}

fn table_border_line(
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    style: Style,
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(*width + 2));
        text.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    indented_line(
        indent,
        vec![Span::styled(text, style.add_modifier(Modifier::DIM))],
    )
}

fn table_separator_line(
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    style: Style,
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push('├');
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&alignment_rule_segment(*width, alignments[index]));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    indented_line(
        indent,
        vec![Span::styled(text, style.add_modifier(Modifier::DIM))],
    )
}

fn alignment_rule_segment(width: usize, alignment: MarkdownAlignment) -> String {
    let inner = width + 2;
    match alignment {
        MarkdownAlignment::Left => format!(":{}", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::Center => {
            if inner <= 2 {
                ":".repeat(inner)
            } else {
                format!(":{}:", "─".repeat(inner - 2))
            }
        }
        MarkdownAlignment::Right => format!("{}:", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::None => "─".repeat(inner),
    }
}

fn render_table_row_wrapped(
    row: &[String],
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    style: Style,
    indent: usize,
) -> Vec<Line<'static>> {
    let wrapped_cells: Vec<Vec<String>> = row
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| wrap_cell_text(cell, *width))
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut lines = Vec::with_capacity(row_height);

    for line_index in 0..row_height {
        let mut text = String::new();
        text.push('│');
        for column_index in 0..widths.len() {
            let cell_line = wrapped_cells[column_index]
                .get(line_index)
                .map(String::as_str)
                .unwrap_or("");
            text.push(' ');
            text.push_str(&pad_aligned(
                cell_line,
                widths[column_index],
                alignments[column_index],
            ));
            text.push(' ');
            text.push('│');
        }
        lines.push(indented_line(indent, vec![Span::styled(text, style)]));
    }

    lines
}

fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for word in raw_line.split_whitespace() {
            let word_width = display_width(word);
            let separator_width = usize::from(!current.is_empty());
            if current_width + separator_width + word_width <= width {
                if separator_width == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(word);
                current_width += word_width;
            } else if current.is_empty() {
                lines.extend(split_word_to_width(word, width));
            } else {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                if word_width <= width {
                    current.push_str(word);
                    current_width = word_width;
                } else {
                    lines.extend(split_word_to_width(word, width));
                }
            }
        }
        if current.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for grapheme in UnicodeSegmentation::graphemes(word, true) {
        let grapheme_width = grapheme_width(grapheme).max(1);
        if !current.is_empty() && current_width + grapheme_width > width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }

        current.push_str(grapheme);
        current_width += grapheme_width;

        if current_width >= width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn pad_aligned(text: &str, width: usize, alignment: MarkdownAlignment) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining - (remaining / 2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        0
    } else {
        UnicodeWidthStr::width(grapheme).max(
            grapheme
                .chars()
                .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                .max()
                .unwrap_or(0),
        )
    }
}

fn inline_plain_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    append_inline_plain_text(inlines, &mut text);
    text
}

fn append_inline_plain_text(inlines: &[MarkdownInline], text: &mut String) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value) | MarkdownInline::Code(value) => text.push_str(value),
            MarkdownInline::Emphasis(content) | MarkdownInline::Strong(content) => {
                append_inline_plain_text(content, text)
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                append_inline_plain_text(content, text);
                if !destination.is_empty() {
                    text.push_str(" (");
                    text.push_str(destination);
                    text.push(')');
                }
            }
            MarkdownInline::Image { alt, destination } => {
                text.push_str("[image: ");
                append_inline_plain_text(alt, text);
                if !destination.is_empty() {
                    text.push_str("] (");
                    text.push_str(destination);
                    text.push(')');
                } else {
                    text.push(']');
                }
            }
            MarkdownInline::LineBreak => text.push('\n'),
        }
    }
}

fn indented_line(indent: usize, mut spans: Vec<Span<'static>>) -> Line<'static> {
    if indent > 0 {
        let mut prefixed = vec![Span::raw(" ".repeat(indent))];
        prefixed.append(&mut spans);
        Line::from(prefixed)
    } else {
        Line::from(spans)
    }
}

fn inlines_to_lines(
    inlines: &[MarkdownInline],
    style: Style,
    indent: usize,
    prefix: Option<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current = if indent > 0 {
        vec![Span::raw(" ".repeat(indent))]
    } else {
        Vec::new()
    };
    if let Some(prefix) = prefix {
        current.push(Span::styled(prefix, style));
    }
    render_inlines_to_lines(inlines, style, &mut lines, &mut current, indent);
    lines.push(Line::from(current));
    lines
}

fn render_inlines_to_lines(
    inlines: &[MarkdownInline],
    style: Style,
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    indent: usize,
) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) => current.push(Span::styled(text.to_string(), style)),
            MarkdownInline::Code(code) => current.push(Span::styled(
                code.to_string(),
                style.add_modifier(Modifier::REVERSED),
            )),
            MarkdownInline::Emphasis(content) => {
                render_inlines_to_lines(
                    content,
                    style.add_modifier(Modifier::ITALIC),
                    lines,
                    current,
                    indent,
                );
            }
            MarkdownInline::Strong(content) => {
                render_inlines_to_lines(
                    content,
                    style.add_modifier(Modifier::BOLD),
                    lines,
                    current,
                    indent,
                );
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_inlines_to_lines(
                    content,
                    style.add_modifier(Modifier::UNDERLINED),
                    lines,
                    current,
                    indent,
                );
                current.push(Span::styled(
                    format!(" ({destination})"),
                    style.add_modifier(Modifier::DIM),
                ));
            }
            MarkdownInline::Image { alt, destination } => {
                current.push(Span::styled("[image: ", style.add_modifier(Modifier::DIM)));
                render_inlines_to_lines(alt, style, lines, current, indent);
                current.push(Span::styled(
                    format!("] ({destination})"),
                    style.add_modifier(Modifier::DIM),
                ));
            }
            MarkdownInline::LineBreak => {
                lines.push(Line::from(std::mem::take(current)));
                if indent > 0 {
                    current.push(Span::raw(" ".repeat(indent)));
                }
            }
        }
    }
}

fn wrapped_line_height(line: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }

    let line_width = display_width(line);
    if line_width == 0 {
        1
    } else {
        line_width.div_ceil(width)
    }
}

fn image_block_height(available_height: usize) -> usize {
    available_height.min(12)
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
            HistoryItem::SessionMessage(_)
            | HistoryItem::StreamingText(_)
            | HistoryItem::Image(_) => {
                panic!("expected text history item")
            }
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
        app.append_stream_text(7, OutputStream::Reasoning, "thinking");
        app.append_stream_text(7, OutputStream::Answer, "hello");
        app.append_stream_text(7, OutputStream::Answer, " world");

        let index = app.in_progress[&7];
        match &app.history[index] {
            HistoryItem::StreamingText(text) => {
                assert_eq!(text.request_id, 7);
                assert_eq!(text.reasoning, "thinking");
                assert_eq!(text.answer, "hello world");
            }
            _ => panic!("expected streaming text item"),
        }
    }

    #[test]
    fn append_stream_text_preserves_manual_scroll_position() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.history_viewport.width = 10;
        app.history_viewport.height = 1;
        app.push_text("older");
        app.push_text("older still");
        app.begin_stream(7);
        app.scroll_up(3);

        app.append_stream_text(7, OutputStream::Answer, "hello");

        assert_eq!(app.history_scroll.scroll(), 3);
        assert_eq!(app.history_scroll.scroll_compensation(), 1);
        assert_eq!(app.effective_scroll(), 4);
        assert!(!app.history_scroll.follow_output());
    }

    #[test]
    fn append_stream_text_keeps_following_when_at_bottom() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.begin_stream(7);

        app.append_stream_text(7, OutputStream::Answer, "hello");

        assert_eq!(app.history_scroll.scroll(), 0);
        assert_eq!(app.history_scroll.scroll_compensation(), 0);
        assert!(app.history_scroll.follow_output());
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
    fn display_width_treats_emoji_as_terminal_cells() {
        assert_eq!(display_width("😀"), 2);
        assert_eq!(display_width("A😀B"), 4);
        assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
    }

    #[test]
    fn split_word_to_width_keeps_emoji_graphemes_together() {
        assert_eq!(split_word_to_width("😀😀", 2), vec!["😀", "😀"]);
        assert_eq!(split_word_to_width("👨‍👩‍👧‍👦x", 2), vec!["👨‍👩‍👧‍👦", "x"]);
    }

    #[test]
    fn wrapped_line_height_uses_terminal_display_width() {
        assert_eq!(wrapped_line_height("😀😀", 2), 2);
        assert_eq!(wrapped_line_height("👨‍👩‍👧‍👦", 2), 1);
    }

    #[test]
    fn streaming_text_lines_include_reasoning_and_answer() {
        let lines = streaming_text_lines(
            &StreamingTextItem {
                request_id: 9,
                reasoning: "step by step".to_string(),
                answer: "final".to_string(),
            },
            80,
        );

        assert_eq!(lines[0].to_string(), "[9]");
        assert_eq!(lines[1].to_string(), "reasoning: step by step");
        assert_eq!(lines[2].to_string(), "answer: final");
    }

    #[test]
    fn streaming_text_lines_preserve_newlines() {
        let lines = streaming_text_lines(
            &StreamingTextItem {
                request_id: 3,
                reasoning: "line one\nline two".to_string(),
                answer: "final one\nfinal two".to_string(),
            },
            80,
        );

        assert_eq!(lines[0].to_string(), "[3]");
        assert_eq!(lines[1].to_string(), "reasoning: line one");
        assert_eq!(lines[2].to_string(), "line two");
        assert_eq!(lines[3].to_string(), "answer: final one");
        assert_eq!(lines[4].to_string(), "final two");
    }

    #[test]
    fn markdown_lines_render_tables() {
        let lines = markdown_lines(
            "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |\n| Grace Hopper | Computer Scientist | 1943 |",
            Style::default(),
            60,
        );

        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("┌"));
        assert!(rendered.contains("Ada Lovelace"));
        assert!(rendered.contains("Grace Hopper"));
        assert!(rendered.contains("Mathematician"));
    }

    #[test]
    fn markdown_lines_render_lists_with_item_text() {
        let lines = markdown_lines(
            "- one\n- [x] done\n1. first\n2. second",
            Style::default(),
            80,
        );

        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("• one"));
        assert!(rendered.contains("• [x] done"));
        assert!(rendered.contains("1. first"));
        assert!(rendered.contains("2. second"));
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
        assert_eq!(image_block_height(0), 0);
        assert_eq!(image_block_height(4), 4);
        assert_eq!(image_block_height(20), 12);
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
        app.history_viewport.height = 1;
        for index in 0..8 {
            app.push_text(format!("line {index}"));
        }
        app.scroll_up(5);

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

        assert_eq!(app.history_scroll.scroll(), 5);
        assert!(!app.history_scroll.follow_output());
    }

    #[test]
    fn scrolling_up_disables_follow_and_scrolling_back_to_bottom_enables_it() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());

        app.scroll_up(3);
        assert_eq!(app.history_scroll.scroll(), 0);
        assert!(app.history_scroll.follow_output());

        app.history_viewport.height = 1;
        app.scroll_up(3);
        assert_eq!(app.history_scroll.scroll(), 1);
        assert!(!app.history_scroll.follow_output());

        app.scroll_down(1);
        assert_eq!(app.history_scroll.scroll(), 0);
        assert!(app.history_scroll.follow_output());
    }

    #[test]
    fn push_text_respects_follow_output_mode() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.history_viewport.width = 10;
        app.history_viewport.height = 1;
        for index in 0..8 {
            app.push_text(format!("line {index}"));
        }
        app.scroll_up(4);

        app.push_text("later");
        assert_eq!(app.history_scroll.scroll(), 4);
        assert_eq!(app.history_scroll.scroll_compensation(), 1);
        assert_eq!(app.effective_scroll(), 5);
        assert!(!app.history_scroll.follow_output());

        app.scroll_down(1);
        assert_eq!(app.history_scroll.scroll(), 4);
        assert_eq!(app.history_scroll.scroll_compensation(), 0);

        app.scroll_down(4);
        app.push_text("latest");
        assert_eq!(app.history_scroll.scroll(), 0);
        assert_eq!(app.history_scroll.scroll_compensation(), 0);
        assert!(app.history_scroll.follow_output());
    }

    #[test]
    fn streaming_growth_above_viewport_preserves_visible_content_offset() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.history_viewport.width = 5;
        app.history_viewport.height = 1;
        app.push_text("older history");
        app.push_text("older history two");
        app.begin_stream(7);
        app.scroll_up(2);

        app.append_stream_text(7, OutputStream::Answer, "123456");

        assert_eq!(app.history_scroll.scroll(), 2);
        assert_eq!(app.history_scroll.scroll_compensation(), 2);
        assert_eq!(app.effective_scroll(), 4);
        assert!(!app.history_scroll.follow_output());
    }

    #[test]
    fn trimming_history_reduces_scroll_by_trimmed_height() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.history_viewport.width = 10;
        app.history_viewport.height = 1;
        app.history_scroll.follow_output = false;
        app.history = (0..499)
            .map(|index| HistoryItem::Text(format!("line {index}")))
            .collect();
        app.history_scroll.scroll = 20;

        app.push_text("tail");
        assert_eq!(app.history_scroll.scroll(), 20);
        assert_eq!(app.history_scroll.scroll_compensation(), 1);
        assert_eq!(app.effective_scroll(), 21);

        app.push_text("tail");

        assert_eq!(app.history.len(), 500);
        assert_eq!(app.history_scroll.scroll(), 20);
        assert_eq!(app.history_scroll.scroll_compensation(), 1);
        assert_eq!(app.effective_scroll(), 21);
        assert!(!app.history_scroll.follow_output());
    }

    #[test]
    fn scrolling_to_top_clamps_without_emptying_history_view() {
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.history_viewport.height = 1;

        app.scroll_up(100);

        assert_eq!(app.max_scroll_offset(), 1);
        assert_eq!(app.effective_scroll(), 1);
        assert_eq!(app.history_scroll.scroll(), 1);
        assert_eq!(app.history_scroll.scroll_compensation(), 0);
        assert!(!app.history_scroll.follow_output());
    }
}
