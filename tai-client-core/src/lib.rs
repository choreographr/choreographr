mod image;
mod markdown;
mod shell;

pub use image::{ImageAssembler, PendingImage};
pub use markdown::{
    MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline, render_markdown_html,
};
pub use shell::{ShellCommand, StreamingText, parse_input_line};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::inline_text;
    use tai_proto::{ClientMessage, ImageMetadata, OutputStream};

    #[test]
    fn parses_empty_line() {
        let mut next = 1;
        assert_eq!(parse_input_line("   ", &mut next), ShellCommand::Empty);
        assert_eq!(next, 1);
    }

    #[test]
    fn parses_ping() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":ping", &mut next),
            ShellCommand::Send(ClientMessage::Ping)
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel 42", &mut next),
            ShellCommand::Send(ClientMessage::Cancel { request_id: 42 })
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn rejects_invalid_cancel() {
        let mut next = 3;
        assert_eq!(
            parse_input_line(":cancel nope", &mut next),
            ShellCommand::InvalidCancel("nope".to_string())
        );
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_test_image_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/image", &mut next),
            ShellCommand::Send(ClientMessage::TestImage { request_id: 10 })
        );
        assert_eq!(next, 11);
    }

    #[test]
    fn parses_models_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models", &mut next),
            ShellCommand::Send(ClientMessage::ListModels)
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_set_model_command() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("/models gpt-5.4-nano", &mut next),
            ShellCommand::Send(ClientMessage::SetModel {
                model: "gpt-5.4-nano".to_string(),
            })
        );
        assert_eq!(next, 10);
    }

    #[test]
    fn parses_run_input_and_increments_request_id() {
        let mut next = 10;
        assert_eq!(
            parse_input_line("hello world", &mut next),
            ShellCommand::Send(ClientMessage::RunInput {
                request_id: 10,
                input: b"hello world".to_vec(),
            })
        );
        assert_eq!(next, 11);
    }

    #[test]
    fn streaming_text_appends_to_matching_stream() {
        let mut entry = StreamingText::new(7);
        entry.append(OutputStream::Reasoning, "thinking");
        entry.append(OutputStream::Answer, "hello");
        entry.append(OutputStream::Answer, " world");

        assert_eq!(entry.request_id, 7);
        assert_eq!(entry.reasoning, "thinking");
        assert_eq!(entry.answer, "hello world");
    }

    #[test]
    fn markdown_parser_supports_common_llm_output() {
        let document = MarkdownDocument::parse(
            "# Heading\n\nA **bold** [link](https://example.com).\n\n- one\n- two\n\n```rs\nfn main() {}\n```",
        );

        assert!(matches!(document.blocks[0], MarkdownBlock::Heading { .. }));
        assert!(matches!(document.blocks[1], MarkdownBlock::Paragraph(_)));
        assert!(matches!(document.blocks[2], MarkdownBlock::List { .. }));
        assert!(matches!(document.blocks[3], MarkdownBlock::CodeBlock { .. }));

        let MarkdownBlock::List { items, .. } = &document.blocks[2] else {
            panic!("expected list block");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(item_plain_text(&items[0]), "one");
        assert_eq!(item_plain_text(&items[1]), "two");
    }

    #[test]
    fn markdown_parser_preserves_task_list_item_text() {
        let document = MarkdownDocument::parse("- [x] done\n- [ ] todo");

        let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
            panic!("expected list block");
        };

        assert_eq!(item_plain_text(&items[0]), "[x] done");
        assert_eq!(item_plain_text(&items[1]), "[ ] todo");
    }

    #[test]
    fn markdown_parser_preserves_nested_tight_list_text() {
        let document = MarkdownDocument::parse("- parent\n  - child\n  - child 2");

        let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
            panic!("expected top-level list block");
        };

        assert_eq!(item_plain_text(&items[0]), "parentchildchild 2");

        let nested_list = items[0]
            .iter()
            .find_map(|block| match block {
                MarkdownBlock::List { items, .. } => Some(items),
                _ => None,
            })
            .expect("expected nested list");
        assert_eq!(item_plain_text(&nested_list[0]), "child");
        assert_eq!(item_plain_text(&nested_list[1]), "child 2");
    }

    #[test]
    fn markdown_parser_supports_tables() {
        let document =
            MarkdownDocument::parse("| Name | Role |\n|:--|--:|\n| Ada | Math |\n| Grace | CS |");

        assert!(matches!(document.blocks[0], MarkdownBlock::Table { .. }));
    }

    #[test]
    fn markdown_html_escapes_unsafe_html_and_links() {
        let safe_html = render_markdown_html("[ok](https://example.com)");
        let unsafe_html = render_markdown_html("[x](javascript:alert(1))");

        assert!(safe_html.contains("https://example.com"));
        assert!(!unsafe_html.contains("javascript:alert(1)"));
        assert!(!unsafe_html.contains("href="));
    }

    #[test]
    fn markdown_html_renders_tables() {
        let html = render_markdown_html("| Name | Role |\n|---|---|\n| Ada | Math |\n| Grace | CS |");

        assert!(html.contains("<table>"));
        assert!(html.contains("<td>Ada</td>"));
        assert!(html.contains("<td>Grace</td>"));
    }

    fn item_plain_text(blocks: &[MarkdownBlock]) -> String {
        let mut text = String::new();
        for block in blocks {
            match block {
                MarkdownBlock::Paragraph(content) | MarkdownBlock::Heading { content, .. } => {
                    text.push_str(&inline_text(content));
                }
                MarkdownBlock::CodeBlock { code, .. } => text.push_str(code),
                MarkdownBlock::BlockQuote(content) => text.push_str(&item_plain_text(content)),
                MarkdownBlock::List { items, .. } => {
                    for item in items {
                        text.push_str(&item_plain_text(item));
                    }
                }
                MarkdownBlock::Table { .. } | MarkdownBlock::Rule => {}
            }
        }
        text
    }

    #[test]
    fn image_assembler_tracks_lifecycle() {
        let mut assembler = ImageAssembler::new();
        let metadata = ImageMetadata {
            image_id: 11,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 4,
            alt: Some("tiny".to_string()),
        };

        assembler.start(7, metadata.clone()).expect("start");
        assembler.push_chunk(7, 11, &[1, 2]).expect("chunk1");
        assembler.push_chunk(7, 11, &[3, 4]).expect("chunk2");
        let (actual_metadata, data) = assembler.finish(7, 11).expect("finish");

        assert_eq!(actual_metadata, metadata);
        assert_eq!(data, vec![1, 2, 3, 4]);
    }
}
