use ratatui::layout::Rect;
use std::{collections::HashSet, io};
use tai_client_core::{ClientHistory, DaemonMessageHandler, HistoryItem as SharedHistoryItem, MAX_HISTORY_ITEMS};
use tai_proto::{ImageMetadata, OutputStream, SessionMessage};
use tai_tui::{
    MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline, RenderedImage,
    StreamingText, build_rendered_image,
};

pub(crate) struct App {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) active: HashSet<u32>,
    pub(crate) client: ClientHistory<Box<RenderedImage>>,
    pub(crate) history_scroll: HistoryScrollState,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    pub(crate) picker: Option<ratatui_image::picker::Picker>,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryViewport {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryScrollState {
    pub(crate) scroll: usize,
    pub(crate) scroll_compensation: usize,
    pub(crate) follow_output: bool,
}

pub(crate) type HistoryItem = SharedHistoryItem<Box<RenderedImage>>;
pub(crate) type StreamingTextItem = StreamingText;

pub(crate) enum UiEvent {
    Daemon(tai_proto::DaemonMessage),
    ReaderClosed,
    Interrupt,
}

impl HistoryViewport {
    pub(crate) fn new() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    pub(crate) fn update(&mut self, area: Rect) {
        self.width = area.width.max(1);
        self.height = area.height;
    }

    pub(crate) fn item_height(&self, item: &HistoryItem) -> usize {
        match item {
            HistoryItem::Text(text) => history_text_height(text, self.width).max(1),
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::Streaming(text) => {
                let lines = streaming_text_lines(text, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::Image(_) => image_block_height(self.height as usize),
        }
    }
}

impl HistoryScrollState {
    pub(crate) fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
            follow_output: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    #[cfg(test)]
    pub(crate) fn scroll_compensation(&self) -> usize {
        self.scroll_compensation
    }

    pub(crate) fn follow_output(&self) -> bool {
        self.follow_output
    }

    fn unclamped_effective_scroll(&self) -> usize {
        self.scroll.saturating_add(self.scroll_compensation)
    }

    pub(crate) fn clamp(&mut self, max_scroll: usize) {
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

    pub(crate) fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    pub(crate) fn preserve_for_growth(
        &mut self,
        old_height: usize,
        new_height: usize,
        max_scroll: usize,
    ) {
        if !self.follow_output && new_height > old_height {
            self.scroll_compensation = self
                .scroll_compensation
                .saturating_add(new_height - old_height);
            self.clamp(max_scroll);
        }
    }

    pub(crate) fn on_item_appended(&mut self, added_height: usize, max_scroll: usize) {
        if self.follow_output {
            self.scroll = 0;
            self.scroll_compensation = 0;
        } else {
            self.scroll_compensation = self.scroll_compensation.saturating_add(added_height);
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        if self.scroll > 0 {
            self.follow_output = false;
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
        if self.scroll == 0 && self.scroll_compensation == 0 {
            self.follow_output = true;
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn account_for_trimmed_height(&mut self, trimmed_height: usize, max_scroll: usize) {
        self.scroll_compensation = self.scroll_compensation.saturating_sub(trimmed_height);
        self.clamp(max_scroll);
    }
}

impl App {
    pub(crate) fn new(socket_path: String, picker_protocol: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            active: HashSet::new(),
            client: ClientHistory::new(vec![
                HistoryItem::Text(format!("Connected to tai-daemon at {socket_path}")),
                HistoryItem::Text(format!("image protocol: {picker_protocol}")),
            ]),
            history_scroll: HistoryScrollState::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            picker: None,
        }
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.client
            .history
            .iter()
            .map(|item| self.history_viewport.item_height(item))
            .sum()
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        let viewport_height = self.history_viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        self.history_scroll.clamp(self.max_scroll_offset());
    }

    pub(crate) fn effective_scroll(&self) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset())
    }

    pub(crate) fn preserve_scroll_for_growth(&mut self, old_height: usize, new_height: usize) {
        self.history_scroll
            .preserve_for_growth(old_height, new_height, self.max_scroll_offset());
    }

    pub(crate) fn push_text(&mut self, line: impl Into<String>) {
        self.push_history_item(HistoryItem::Text(line.into()));
    }

    pub(crate) fn push_session_message(&mut self, message: SessionMessage) {
        self.push_history_item(HistoryItem::SessionMessage(message));
    }

    pub(crate) fn push_image(&mut self, image: RenderedImage) {
        let item = HistoryItem::Image(Box::new(image));
        self.push_history_item(item);
    }

    pub(crate) fn push_history_item(&mut self, item: HistoryItem) {
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.push_history_item(item);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn begin_stream(&mut self, request_id: u32) {
        if self.client.in_progress.contains_key(&request_id) {
            return;
        }
        let item = HistoryItem::Streaming(StreamingTextItem::new(request_id));
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.begin_stream(request_id);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn append_stream_text(
        &mut self,
        request_id: u32,
        stream: OutputStream,
        chunk: &str,
    ) {
        if !self.client.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.client.in_progress.get(&request_id) {
            let old_height = self
                .client
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(0);
            self.client.append_stream(request_id, stream, chunk);
            let new_height = self
                .client
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(old_height);
            self.preserve_scroll_for_growth(old_height, new_height);
        }
    }

    pub(crate) fn finalize_stream(&mut self, request_id: u32) {
        self.client.in_progress.remove(&request_id);
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset());
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset());
    }

    fn trimmed_height_on_append(&self) -> usize {
        if self.client.history.len() < MAX_HISTORY_ITEMS || self.history_scroll.follow_output() {
            return 0;
        }
        self.client
            .history
            .first()
            .map(|item| self.history_viewport.item_height(item))
            .unwrap_or(0)
    }

    fn account_for_trimmed_height(&mut self, trimmed_height: usize) {
        self.history_scroll
            .account_for_trimmed_height(trimmed_height, self.max_scroll_offset());
    }
}

impl DaemonMessageHandler for App {
    fn push_text(&mut self, text: String) {
        self.push_text(text);
    }

    fn push_session_message(&mut self, message: SessionMessage) {
        self.push_session_message(message);
    }

    fn begin_stream(&mut self, request_id: u32) {
        self.begin_stream(request_id);
    }

    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        self.append_stream_text(request_id, stream, chunk);
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.finalize_stream(request_id);
    }

    fn drop_request(&mut self, request_id: u32) {
        self.active.remove(&request_id);
        self.client.in_progress.remove(&request_id);
        self.client.pending_images.drop_request(request_id);
    }

    fn handle_image_start(&mut self, request_id: u32, metadata: ImageMetadata) -> io::Result<()> {
        self.client.start_image(request_id, metadata)
    }

    fn handle_image_chunk(&mut self, request_id: u32, image_id: u32, data: &[u8]) -> io::Result<()> {
        self.client.push_image_chunk(request_id, image_id, data)
    }

    fn handle_image_end(&mut self, request_id: u32, image_id: u32) -> io::Result<()> {
        let (metadata, data) = self.client.finish_image(request_id, image_id)?;
        let picker = self.picker.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "image picker not set")
        })?;
        let rendered = build_rendered_image(picker, metadata, data)?;
        self.push_image(rendered);
        Ok(())
    }
}

pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    let lines = plain_text_lines(text);
    lines_height(&lines, width)
}

pub(crate) fn lines_height(lines: &[String], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if lines.len() <= 1 && lines.iter().all(|line| line.is_empty()) {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
        .max(1)
}

pub(crate) fn plain_text_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        vec![String::new()]
    } else {
        text.split('\n').map(|line| line.to_string()).collect()
    }
}

pub(crate) fn session_message_lines(message: &SessionMessage, width: u16) -> Vec<String> {
    match message {
        SessionMessage::SystemText { content } => labeled_plain_text_lines("system", content),
        SessionMessage::UserText { content } => labeled_plain_text_lines("user", content),
        SessionMessage::AssistantText { content } => {
            let body = markdown_lines(content, width);
            prefixed_lines("assistant", body)
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning_content,
            reasoning,
            reasoning_text,
        } => {
            let mut lines = vec![format!(
                "tool-call: {}",
                tool_calls
                    .iter()
                    .map(|call| format!("{}({})", call.name, call.arguments_json))
                    .collect::<Vec<_>>()
                    .join(", ")
            )];
            if let Some(reasoning_text) = reasoning_content
                .as_deref()
                .or(reasoning.as_deref())
                .or(reasoning_text.as_deref())
                .filter(|value| !value.trim().is_empty())
            {
                append_section(&mut lines, "reasoning", plain_text_lines(reasoning_text));
            }
            if let Some(content) = content.as_deref().filter(|value| !value.trim().is_empty()) {
                append_section(&mut lines, "content", markdown_lines(content, width));
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
            prefixed_lines(label, plain_text_lines(&format!("{name}: {content}")))
        }
    }
}

pub(crate) fn streaming_text_lines(text: &StreamingTextItem, width: u16) -> Vec<String> {
    let mut lines = vec![format!("[{}]", text.request_id)];

    if !text.reasoning.is_empty() {
        append_section(&mut lines, "reasoning", plain_text_lines(&text.reasoning));
    }

    if !text.answer.is_empty() {
        append_section(&mut lines, "answer", markdown_lines(&text.answer, width));
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn labeled_plain_text_lines(label: &'static str, text: &str) -> Vec<String> {
    prefixed_lines(label, plain_text_lines(text))
}

fn prefixed_lines(label: &'static str, body: Vec<String>) -> Vec<String> {
    let mut lines = Vec::new();
    append_section(&mut lines, label, body);
    lines
}

fn append_section(lines: &mut Vec<String>, label: &'static str, body: Vec<String>) {
    let mut body_iter = body.into_iter();
    if let Some(first) = body_iter.next() {
        lines.push(format!("{label}: {first}"));
    } else {
        lines.push(format!("{label}:"));
    }
    lines.extend(body_iter);
}

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<String> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, 0, width as usize);
    if lines.is_empty() {
        lines.push(String::new());
    }
    while matches!(lines.last(), Some(line) if line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<String>,
    indent: usize,
    width: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        render_markdown_block(block, lines, indent, width);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<String>,
    indent: usize,
    width: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => lines.extend(inlines_to_lines(content, indent, None)),
        MarkdownBlock::Heading { level, content } => {
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            lines.extend(inlines_to_lines(content, indent, prefix));
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));
            for line in code.split('\n') {
                lines.push(indented_line(indent, line.to_string()));
            }
            lines.push(indented_line(indent, "```".to_string()));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            render_markdown_blocks(blocks, &mut quoted, 0, width);
            for line in quoted {
                lines.push(indented_line(indent, format!("> {line}")));
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
                render_markdown_blocks(item, &mut rendered, 0, width);
                let mut rendered_iter = rendered.into_iter();
                if let Some(first) = rendered_iter.next() {
                    lines.push(format!("{}{}{}", " ".repeat(indent), marker, first));
                } else {
                    lines.push(format!("{}{}", " ".repeat(indent), marker));
                }
                for line in rendered_iter {
                    lines.push(format!("{}{}", " ".repeat(continuation_indent), line));
                }
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => lines.extend(render_table_lines(alignments, header, rows, indent, width)),
        MarkdownBlock::Rule => lines.push(indented_line(indent, "---".to_string())),
    }
}

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> Vec<String> {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return vec![String::new()];
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
    let header_alignment = normalized_alignments(alignments, column_count);
    let mut lines = Vec::new();
    lines.push(table_border_line('┌', '┬', '┐', &widths, indent));
    lines.extend(render_table_row_wrapped(
        &table_rows[0],
        &widths,
        &header_alignment,
        indent,
    ));
    lines.push(table_separator_line(&widths, &header_alignment, indent));
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        lines.extend(render_table_row_wrapped(
            row,
            &widths,
            &header_alignment,
            indent,
        ));
        if index < table_rows.len() - 1 {
            lines.push(table_border_line('├', '┼', '┤', &widths, indent));
        }
    }
    lines.push(table_border_line('└', '┴', '┘', &widths, indent));
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
    indent: usize,
) -> String {
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
    indented_line(indent, text)
}

fn table_separator_line(
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
) -> String {
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
    indented_line(indent, text)
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
    indent: usize,
) -> Vec<String> {
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
        lines.push(indented_line(indent, text));
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
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(word, true) {
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

pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        0
    } else {
        unicode_width::UnicodeWidthStr::width(grapheme).max(
            grapheme
                .chars()
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
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

fn indented_line(indent: usize, text: String) -> String {
    if indent > 0 {
        format!("{}{}", " ".repeat(indent), text)
    } else {
        text
    }
}

fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Option<String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    if indent > 0 {
        current.push_str(&" ".repeat(indent));
    }
    if let Some(prefix) = prefix {
        current.push_str(&prefix);
    }
    render_inlines_to_lines(inlines, &mut lines, &mut current, indent);
    lines.push(current);
    lines
}

fn render_inlines_to_lines(
    inlines: &[MarkdownInline],
    lines: &mut Vec<String>,
    current: &mut String,
    indent: usize,
) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) | MarkdownInline::Code(text) => current.push_str(text),
            MarkdownInline::Emphasis(content) | MarkdownInline::Strong(content) => {
                render_inlines_to_lines(content, lines, current, indent)
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_inlines_to_lines(content, lines, current, indent);
                current.push_str(&format!(" ({destination})"));
            }
            MarkdownInline::Image { alt, destination } => {
                current.push_str("[image: ");
                render_inlines_to_lines(alt, lines, current, indent);
                current.push_str(&format!("] ({destination})"));
            }
            MarkdownInline::LineBreak => {
                lines.push(std::mem::take(current));
                if indent > 0 {
                    current.push_str(&" ".repeat(indent));
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

pub(crate) fn image_block_height(available_height: usize) -> usize {
    available_height.min(12)
}
