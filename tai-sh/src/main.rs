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
    ClientMessage, DaemonMessage, OutputStream, read_message, socket_path, write_message,
};
use tai_sh::{
    ImageAssembler, RenderedImage, ShellCommand, StreamingText, build_picker, build_rendered_image,
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
            HistoryItem::StreamingText(text) => {
                let lines = streaming_text_lines(text);
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
        let item = HistoryItem::Text(line.into());
        let added_height = self.history_viewport.item_height(&item);
        self.history.push(item);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.trim_history();
        self.clamp_scroll_state();
    }

    fn push_image(&mut self, image: RenderedImage) {
        let item = HistoryItem::Image(image);
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
) -> io::Result<()> {
    match message {
        DaemonMessage::Started { request_id } => {
            app.begin_stream(request_id);
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
        .saturating_add(1 + app.input.chars().count() as u16);
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
            HistoryItem::StreamingText(text) => {
                let lines = streaming_text_lines(text);
                let wrapped = lines_height(&lines, area.width).max(1);
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
                    Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .scroll((top_line as u16, 0)),
                    rect,
                );
                rows_remaining -= visible_height;
                rows_to_skip = 0;
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

fn history_text_height(text: &str, width: u16) -> usize {
    let lines: Vec<Line<'_>> = if text.is_empty() {
        vec![Line::from("")]
    } else {
        text.split('\n').map(Line::from).collect()
    };
    lines_height(&lines, width)
}

fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    let text = lines.iter().map(Line::width).sum::<usize>();
    if text == 0 && lines.len() <= 1 {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(&line.to_string(), width))
        .sum::<usize>()
        .max(1)
}

fn streaming_text_lines(text: &StreamingTextItem) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!("[{}]", text.request_id))];

    if !text.reasoning.is_empty() {
        append_labeled_lines(
            &mut lines,
            "reasoning: ",
            &text.reasoning,
            Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
        );
    }

    if !text.answer.is_empty() {
        append_labeled_lines(&mut lines, "answer: ", &text.answer, Style::default());
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(Line::from(""));
    }

    lines
}

fn append_labeled_lines(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    text: &str,
    style: Style,
) {
    let mut split = text.split('\n');
    if let Some(first) = split.next() {
        lines.push(Line::from(vec![
            Span::styled(label, style),
            Span::styled(first.to_string(), style),
        ]));
    }

    for line in split {
        lines.push(Line::from(Span::styled(line.to_string(), style)));
    }
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
            HistoryItem::StreamingText(_) | HistoryItem::Image(_) => {
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
    fn streaming_text_lines_include_reasoning_and_answer() {
        let lines = streaming_text_lines(&StreamingTextItem {
            request_id: 9,
            reasoning: "step by step".to_string(),
            answer: "final".to_string(),
        });

        assert_eq!(lines[0].to_string(), "[9]");
        assert_eq!(lines[1].to_string(), "reasoning: step by step");
        assert_eq!(lines[2].to_string(), "answer: final");
    }

    #[test]
    fn streaming_text_lines_preserve_newlines() {
        let lines = streaming_text_lines(&StreamingTextItem {
            request_id: 3,
            reasoning: "line one\nline two".to_string(),
            answer: "final one\nfinal two".to_string(),
        });

        assert_eq!(lines[0].to_string(), "[3]");
        assert_eq!(lines[1].to_string(), "reasoning: line one");
        assert_eq!(lines[2].to_string(), "line two");
        assert_eq!(lines[3].to_string(), "answer: final one");
        assert_eq!(lines[4].to_string(), "final two");
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
