//! Tests for the parent module; kept alongside the implementation.
use super::{
    coalesce_rich_spans, compact_bare_list_markers, compact_decorative_horizontal_rules,
    compact_generic_image_alt_lines, compact_prose_whitespace, compact_social_link_chrome,
    extract_thread_headers, parse_body, parse_summary, read_all_limited, read_header_block,
    render_html_for_terminal, simplify_layout_table_blocks,
};
use crate::model::{HtmlLine, HtmlSpan, HtmlSpanStyle, MessageFlags, MessageId};
use std::io::Cursor;

const MAIL: &[u8] = b"From: Example Person <person@example.com>\r\n\
Subject: [PATCH v2 1/2] usb: fix thing\r\n\
Date: Thu, 22 Oct 2026 01:17:54 +0200\r\n\
Message-ID: <patch-1@example.com>\r\n\
In-Reply-To: <cover@example.com>\r\n\
References: <root@example.com>\r\n\t<cover@example.com>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello from the body.\r\n";

#[test]
fn rich_span_coalescing_preserves_text_styles_links_and_images() {
    let plain = HtmlSpanStyle::default();
    let linked = HtmlSpanStyle {
        link: Some("https://example.test/one".into()),
        ..HtmlSpanStyle::default()
    };
    let other_link = HtmlSpanStyle {
        link: Some("https://example.test/two".into()),
        ..HtmlSpanStyle::default()
    };
    let image = HtmlSpanStyle {
        image: Some("cid:logo".into()),
        ..HtmlSpanStyle::default()
    };
    let bold = HtmlSpanStyle {
        bold: true,
        ..HtmlSpanStyle::default()
    };
    let span = |text: &str, style: &HtmlSpanStyle| HtmlSpan {
        text: text.into(),
        style: style.clone(),
    };
    let mut lines = vec![
        HtmlLine {
            spans: vec![
                span("Hel", &plain),
                span("lo ", &plain),
                span("😀", &plain),
                span("click", &linked),
                span(" here", &linked),
                span("!", &other_link),
                span("[img]", &image),
                span("[img]", &image),
                span("B", &bold),
                span("old", &bold),
            ],
        },
        HtmlLine::default(),
    ];
    let original_text = super::rich_lines_to_text(&lines);
    coalesce_rich_spans(&mut lines);
    assert_eq!(super::rich_lines_to_text(&lines), original_text);
    assert_eq!(lines.len(), 2, "line boundaries must not change");
    assert_eq!(lines[0].spans.len(), 6);
    assert_eq!(lines[0].spans[0].text, "Hello 😀");
    assert_eq!(lines[0].spans[1].text, "click here");
    assert_eq!(lines[0].spans[1].style, linked);
    assert_eq!(lines[0].spans[2].style, other_link);
    assert_eq!(lines[0].spans[3].style, image);
    assert_eq!(lines[0].spans[4].style, image);
    assert_eq!(lines[0].spans[5].text, "Bold");
    assert_eq!(lines[0].spans[5].style, bold);
    assert!(lines[1].spans.is_empty());
}

#[test]
fn rich_span_coalescing_is_idempotent_and_does_not_merge_different_styles() {
    let mut lines = vec![HtmlLine {
        spans: vec![
            HtmlSpan {
                text: "A".into(),
                style: HtmlSpanStyle::default(),
            },
            HtmlSpan {
                text: "B".into(),
                style: HtmlSpanStyle {
                    italic: true,
                    ..HtmlSpanStyle::default()
                },
            },
            HtmlSpan {
                text: "C".into(),
                style: HtmlSpanStyle::default(),
            },
            HtmlSpan {
                text: "│".into(),
                style: HtmlSpanStyle::default(),
            },
            HtmlSpan {
                text: "D".into(),
                style: HtmlSpanStyle::default(),
            },
        ],
    }];
    coalesce_rich_spans(&mut lines);
    let once = lines.clone();
    coalesce_rich_spans(&mut lines);
    assert_eq!(lines, once);
    assert_eq!(lines[0].spans.len(), 5, "table borders must stay separate");
}

#[test]
fn rich_html_renderer_emits_no_mergeable_adjacent_spans() {
    let (text, lines, _) = render_html_for_terminal(
        "<p>Hello <strong>bold text</strong> and <a href='https://example.test'>a link</a>.</p>",
        80,
    );
    assert!(text.contains("Hello"));
    for line in &lines {
        for pair in line.spans.windows(2) {
            assert!(
                pair[0].style != pair[1].style
                    || pair[0].style.image.is_some()
                    || pair.iter().any(|span| {
                        span.text
                            .chars()
                            .any(|ch| ('\u{2500}'..='\u{257f}').contains(&ch))
                    }),
                "rendered HTML still contains coalescible spans"
            );
        }
    }
}

#[test]
fn streaming_reader_rejects_input_larger_than_budget() {
    let ok = read_all_limited(Cursor::new(b"1234"), 4).unwrap();
    assert_eq!(ok, b"1234");
    assert!(read_all_limited(Cursor::new(b"12345"), 4).is_err());
}

#[test]
fn old_cached_calendar_hints_are_collapsed_for_list_and_reader() {
    let hints = vec!["attachment-1".into(), "invite.ics".into()];
    assert_eq!(super::visible_attachment_hints(&hints), vec!["invite.ics"]);
    assert_eq!(hints.len(), 2, "do not mutate cached source data");
    assert_eq!(
        super::visible_attachment_hints(&["attachment-1".into()]),
        vec!["attachment-1"]
    );
    assert_eq!(
        super::visible_attachment_hints(&[
            "attachment-1".into(),
            "invite.ics".into(),
            "report.pdf".into(),
        ])
        .len(),
        3,
        "never guess about larger multipart messages"
    );
}

#[test]
fn duplicate_calendar_part_shows_only_named_ics_attachment() {
    // Matches the structure of Google Calendar invitations: a calendar
    // alternate alongside the exact same bytes named invite.ics.
    let raw = b"From: sender@example.org\r\nSubject: Event\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n\
--inner\r\nContent-Type: text/plain\r\n\r\nInvitation\r\n\
--inner--\r\n\
--outer\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer\r\nContent-Type: application/ics; name=invite.ics\r\n\
Content-Disposition: attachment; filename=invite.ics\r\n\r\n\
BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer--\r\n";
    let attachments = super::extract_attachments(raw).unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "invite.ics");
    let body = parse_body(MessageId("calendar".into()), raw).unwrap();
    assert_eq!(body.attachment_names, vec!["invite.ics"]);
}

#[test]
fn standalone_calendar_part_still_appears_as_invite_ics() {
    let raw = b"From: sender@example.org\r\nSubject: Event\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
--outer\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
--outer\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--outer--\r\n";
    let files = super::extract_attachments(raw).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].filename, "invite.ics");
}

#[test]
fn header_reader_stops_before_body() {
    let headers = read_header_block(Cursor::new(MAIL)).unwrap();
    let text = String::from_utf8(headers).unwrap();
    assert!(text.contains("Subject:"));
    assert!(!text.contains("Hello from the body"));
}

#[test]
fn extracts_threading_metadata() {
    let headers = read_header_block(Cursor::new(MAIL)).unwrap();
    let t = extract_thread_headers(&headers);
    assert_eq!(t.message_id.as_deref(), Some("patch-1@example.com"));
    assert_eq!(t.in_reply_to.as_deref(), Some("cover@example.com"));
    assert_eq!(t.references, ["root@example.com", "cover@example.com"]);
}

#[test]
fn plain_only_mail_does_not_invent_an_html_toggle() {
    let body = parse_body(MessageId("plain".into()), MAIL).unwrap();
    assert!(body.plain_text.is_some());
    assert!(body.html_text.is_none());
    assert!(body.html_lines.is_none());
}

#[test]
fn html_only_mail_uses_terminal_html_renderer() {
    let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Hello</h1><p>World</p></body></html>";
    let body = parse_body(MessageId("html".into()), raw).unwrap();
    assert!(body.plain_text.is_none());
    assert!(
        body.html_text
            .as_deref()
            .is_some_and(|text| text.contains("Hello"))
    );
    assert!(
        body.html_lines
            .as_ref()
            .is_some_and(|lines| !lines.is_empty())
    );
}

#[test]
fn html_reader_mode_preserves_table_structure_and_links() {
    let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<table><tr><td><a href=\"https://tracking.example/very/long/url\">View in Figma</a></td><td><strong>Open</strong></td></tr><tr><td>Comment text</td><td>More</td></tr></table>";
    let body = parse_body(MessageId("html-reader".into()), raw).unwrap();
    let rendered = body.html_text.as_deref().unwrap();
    assert!(rendered.contains("View in Figma"));
    assert!(rendered.contains("Comment text"));
    assert!(
        rendered
            .chars()
            .any(|ch| matches!(ch, '│' | '─' | '┌' | '┐'))
    );
    assert_eq!(
        body.html_links(),
        vec!["https://tracking.example/very/long/url".to_owned()]
    );
    assert_eq!(body.html_renderer.as_deref(), Some("html2text-rich"));
}

#[test]
fn html_reader_renders_explicit_superscript_ordinals() {
    // Mirrors the Outlook-generated HTML of a course-assessment email.
    let raw = b"From: sender@example.com\r\nSubject: Assessments\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>The deadline is January 8<sup>th</sup>, 2027.</p><p>Results January 15<SUP class=\"ordinal\">th</SUP>, 2027.</p>";
    let body = parse_body(MessageId("ordinal-html".into()), raw).unwrap();
    let rendered = body.html_text.as_deref().unwrap();
    assert!(rendered.contains("January 8ᵗʰ, 2027"), "{rendered}");
    assert!(rendered.contains("January 15ᵗʰ, 2027"), "{rendered}");
    assert!(
        body.html_source
            .as_deref()
            .is_some_and(|source| source.contains("8<sup>th</sup>")),
        "original HTML must remain unchanged for forwarding and reflow"
    );
    // Reflowing from original HTML must produce identical ordinal glyphs.
    let (reflow, _, _) = render_html_for_terminal(body.html_source.as_deref().unwrap(), 60);
    assert!(reflow.contains("January 8ᵗʰ"), "{reflow}");
    assert!(reflow.contains("January 15ᵗʰ"), "{reflow}");
}

#[test]
fn superscript_normalizer_preserves_plain_text_and_other_html() {
    let html = "15th, 1<sup>st</sup>, 2<sup>nd</sup>, 3<sup>rd</sup>, E<sup>mc2</sup>, X<sup><em>th</em></sup>";
    assert_eq!(
        super::normalize_superscript_ordinals(html).as_ref(),
        "15th, 1ˢᵗ, 2ⁿᵈ, 3ʳᵈ, E<sup>mc2</sup>, X<sup><em>th</em></sup>"
    );
    assert!(matches!(
        super::normalize_superscript_ordinals("plain 15th and no superscripts"),
        std::borrow::Cow::Borrowed(_)
    ));
    assert!(matches!(
        super::normalize_superscript_ordinals("<sup>unrecognized</sup>"),
        std::borrow::Cow::Borrowed(_)
    ));
}

#[test]
fn html_reader_preserves_supported_css_colours() {
    let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p style=\"color:#ff0000;background-color:#0000ff\"><a href=\"https://example.com\">Colour link</a></p>";
    let body = parse_body(MessageId("html-colour".into()), raw).unwrap();
    let styles = body
        .html_lines
        .as_deref()
        .into_iter()
        .flatten()
        .flat_map(|line| line.spans.iter())
        .map(|span| &span.style)
        .collect::<Vec<_>>();
    assert!(
        styles
            .iter()
            .any(|style| style.foreground == Some((255, 0, 0)))
    );
    assert!(
        styles
            .iter()
            .any(|style| style.background == Some((0, 0, 255)))
    );
    assert!(
        styles
            .iter()
            .any(|style| style.link.as_deref() == Some("https://example.com"))
    );
}

#[test]
fn html_reader_strips_zero_width_preheader_padding() {
    let raw = "From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>\u{200c}\u{200c}Visible</p>";
    let body = parse_body(MessageId("html-zero-width".into()), raw.as_bytes()).unwrap();
    assert_eq!(body.html_text.as_deref().map(str::trim), Some("Visible"));
}

#[test]
fn html_reader_mode_collapses_excess_blank_lines() {
    let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>One</p><br><br><br><p>Two</p>";
    let body = parse_body(MessageId("html-space".into()), raw).unwrap();
    let rendered = body.html_text.unwrap();
    assert!(!rendered.contains("\n\n\n"));
}

#[test]
fn html_reader_removes_repeated_layout_table_rules_but_keeps_real_table_chrome() {
    let line = |text: &str| HtmlLine {
        spans: vec![HtmlSpan {
            text: text.to_owned(),
            style: HtmlSpanStyle::default(),
        }],
    };
    let compacted = compact_decorative_horizontal_rules(vec![
        line("────────────────"),
        line("Hello"),
        line("────────────────"),
        line("World"),
        line("────────────────"),
        line("┌────┬────┐"),
        line("│ A  │ B  │"),
        line("────────────────"),
    ]);
    let text = compacted
        .iter()
        .map(super::html_line_text)
        .collect::<Vec<_>>();
    assert!(!text.iter().any(|line| line == "────────────────"));
    assert!(text.iter().any(|line| line == "┌────┬────┐"));
    assert!(text.iter().any(|line| line == "│ A  │ B  │"));
}

#[test]
fn social_footer_links_compact_without_losing_targets() {
    let social = ["Facebook", "Instagram", "YouTube"]
        .into_iter()
        .map(|label| HtmlLine {
            spans: vec![HtmlSpan {
                text: label.to_owned(),
                style: HtmlSpanStyle {
                    link: Some(format!("https://example.com/{label}")),
                    ..HtmlSpanStyle::default()
                },
            }],
        })
        .collect::<Vec<_>>();
    let compacted = compact_social_link_chrome(social);
    assert_eq!(compacted.len(), 1);
    let text = compacted[0]
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert_eq!(text, "[links: Facebook · Instagram · YouTube]");
    assert_eq!(
        compacted[0]
            .spans
            .iter()
            .filter(|span| span.style.link.is_some())
            .count(),
        3
    );
}

#[test]
fn horizontal_social_footer_table_compacts_without_losing_targets() {
    let separator = |text: &str| HtmlSpan {
        text: text.to_owned(),
        style: HtmlSpanStyle::default(),
    };
    let social = |label: &str| HtmlSpan {
        text: label.to_owned(),
        style: HtmlSpanStyle {
            link: Some(format!("https://example.com/{label}")),
            image: Some(format!("https://example.com/{label}.png")),
            ..HtmlSpanStyle::default()
        },
    };
    let lines = vec![
        HtmlLine {
            spans: vec![separator("────────┬───────┬────────")],
        },
        HtmlLine {
            spans: vec![
                social("Facebook"),
                separator(" │ "),
                social("X"),
                separator(" │ "),
                social("Instagram"),
            ],
        },
        HtmlLine {
            spans: vec![separator("────────┴───────┴────────")],
        },
    ];

    let compacted = compact_social_link_chrome(lines);
    assert_eq!(compacted.len(), 1);
    assert_eq!(
        compacted[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "[links: Facebook · X · Instagram]"
    );
    assert_eq!(
        compacted[0]
            .spans
            .iter()
            .filter(|span| span.style.link.is_some())
            .count(),
        3
    );
}

#[test]
fn generic_image_alt_placeholders_are_removed_but_descriptive_images_survive() {
    let image_line = |label: &str| HtmlLine {
        spans: vec![HtmlSpan {
            text: label.to_owned(),
            style: HtmlSpanStyle {
                image: Some("https://example.com/image.png".to_owned()),
                ..HtmlSpanStyle::default()
            },
        }],
    };
    let compacted = compact_generic_image_alt_lines(vec![
        image_line("Dynamic Banner"),
        image_line("item image"),
        image_line("June Bluefin Tuna Cutting Ceremony"),
    ]);
    assert_eq!(compacted.len(), 1);
    assert_eq!(
        compacted[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "June Bluefin Tuna Cutting Ceremony"
    );
}

#[test]
fn repeated_bare_list_markers_are_removed() {
    let line = |text: &str| HtmlLine {
        spans: vec![HtmlSpan {
            text: text.to_owned(),
            style: HtmlSpanStyle::default(),
        }],
    };
    let compacted = compact_bare_list_markers(vec![
        line("* First item"),
        line("*"),
        line("* Second item"),
        line("*"),
        line("* Third item"),
    ]);
    let text = compacted
        .iter()
        .map(super::html_line_text)
        .collect::<Vec<_>>();
    assert_eq!(text, ["* First item", "* Second item", "* Third item"]);
}

#[test]
fn prose_spaces_compact_without_changing_table_alignment() {
    let line = |text: &str| HtmlLine {
        spans: vec![HtmlSpan {
            text: text.to_owned(),
            style: HtmlSpanStyle::default(),
        }],
    };
    let table = "│ A    │ B  │";
    let compacted = compact_prose_whitespace(vec![
        line("Please don't miss the   first cut!"),
        line(table),
    ]);
    assert_eq!(
        super::html_line_text(&compacted[0]),
        "Please don't miss the first cut!"
    );
    assert_eq!(super::html_line_text(&compacted[1]), table);
}

#[test]
fn html_headings_keep_emphasis_without_literal_markdown_markers() {
    let (text, lines, _) = render_html_for_terminal(
        "<h1>Your Services Agreement made clearer</h1><p>Hello</p>",
        80,
    );
    assert!(text.contains("Your Services Agreement made clearer"));
    assert!(!text.contains("# Your Services Agreement"));
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| { span.text.contains("Your Services Agreement") && span.style.bold })
    );
}

#[test]
fn explicit_presentation_tables_are_linearized_before_rich_rendering() {
    let html = r#"
            <table role = 'presentation'><tr>
              <td><strong>Logo</strong></td><td>Order #580<br>$10.38</td>
            </tr></table>
            <table><tr><td>Ticket</td><td>$85.00</td></tr></table>
        "#;
    let (text, _, _) = render_html_for_terminal(html, 80);
    assert!(text.contains("Logo"));
    assert!(text.contains("Order #580"));
    // The role=presentation table is layout, not data, so it should not
    // contribute box-drawing separators. The unmarked receipt table still
    // should.
    assert_eq!(text.matches('┬').count(), 1);
    assert!(text.contains("Ticket"));
}

#[test]
fn presentation_table_blocks_flatten_without_touching_small_data_tables() {
    let line = |text: &str| HtmlLine {
        spans: vec![HtmlSpan {
            text: text.to_owned(),
            style: HtmlSpanStyle::default(),
        }],
    };
    let flattened = simplify_layout_table_blocks(vec![
        line("─┬────────────────┬─"),
        line(" │Share Your Thoughts│ "),
        line(" │────────────────│ "),
        line(" │                │ "),
        line(" │────────────────│ "),
        line(" │Take Survey     │ "),
        line("─┴────────────────┴─"),
    ]);
    let text = flattened
        .iter()
        .map(super::html_line_text)
        .collect::<Vec<_>>();
    assert!(text.iter().any(|line| line.contains("Share Your Thoughts")));
    assert!(text.iter().any(|line| line.contains("Take Survey")));
    assert!(
        !text
            .iter()
            .any(|line| line.contains('┬') || line.contains('┴'))
    );

    let data = simplify_layout_table_blocks(vec![
        line("──────┬────────"),
        line("Ticket│$85.00"),
        line("──────┴────────"),
    ]);
    assert!(
        data.iter()
            .any(|line| super::html_line_text(line).contains('┬'))
    );
}

#[test]
fn html_preprocessor_borrows_unchanged_markup_and_handles_mixed_case_roles() {
    let ordinary = "<table><tr><td>Receipt</td></tr></table>";
    let unchanged = super::flatten_presentation_tables_html(ordinary);
    assert!(matches!(&unchanged, std::borrow::Cow::Borrowed(_)));
    assert_eq!(unchanged, ordinary);

    // A word in the body must not trigger rewriting unrelated data tables.
    let ordinary = "<p>Presentation details</p><table><tr><td>$85</td></tr></table>";
    assert!(matches!(
        super::flatten_presentation_tables_html(ordinary),
        std::borrow::Cow::Borrowed(_)
    ));

    let marked = "<TABLE ROLE = 'PreSentation'><TR><TD>Logo</TD></TR></TABLE>\
                      <table><tr><td>Receipt</td></tr></table>";
    let normalized = super::flatten_presentation_tables_html(marked);
    assert!(matches!(&normalized, std::borrow::Cow::Owned(_)));
    assert!(normalized.starts_with("<div><div><div>Logo</div></div></div>"));
    assert!(normalized.contains("<table><tr><td>Receipt</td></tr></table>"));
}

#[test]
fn html_cleanup_preserves_styled_links_and_collapses_whitespace_across_spans() {
    let url = "https://example.com/article";
    let linked = HtmlSpanStyle {
        link: Some(url.into()),
        bold: true,
        ..HtmlSpanStyle::default()
    };
    let lines = vec![HtmlLine {
        spans: vec![
            HtmlSpan {
                text: "Read  ".into(),
                style: linked.clone(),
            },
            HtmlSpan {
                text: "\tmore\u{200b}".into(),
                style: linked.clone(),
            },
        ],
    }];
    let clean = compact_prose_whitespace(super::clean_rich_lines(lines));
    assert_eq!(super::html_line_text(&clean[0]), "Read more");
    assert!(clean[0].spans.iter().all(|span| span.style == linked));
    // Multiple cleanup passes should be idempotent, including blank rows.
    assert_eq!(super::clean_rich_lines(clean.clone()), clean);
}

#[test]
fn ordinary_layout_blocks_keep_their_styled_spans() {
    let first = HtmlLine {
        spans: vec![
            HtmlSpan {
                text: "Hello ".into(),
                style: HtmlSpanStyle {
                    italic: true,
                    ..HtmlSpanStyle::default()
                },
            },
            HtmlSpan {
                text: "world".into(),
                style: HtmlSpanStyle {
                    link: Some("https://example.org".into()),
                    ..HtmlSpanStyle::default()
                },
            },
        ],
    };
    let second = HtmlLine {
        spans: vec![HtmlSpan {
            text: "Next paragraph".into(),
            style: HtmlSpanStyle::default(),
        }],
    };
    let actual =
        simplify_layout_table_blocks(vec![first.clone(), HtmlLine::default(), second.clone()]);
    assert_eq!(actual, vec![first, HtmlLine::default(), second]);
}

#[test]
fn html_only_mail_gets_a_plain_toggle_fallback() {
    let raw = b"From: sender@example.com\r\nSubject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h1>Hello</h1><p>Fallback body</p>";
    let body = parse_body(MessageId("html-fallback".into()), raw).unwrap();
    assert!(body.has_html_alternative());
    assert!(body.has_plain_view());
    assert!(body.display_text(false).contains("Fallback body"));
    assert!(body.display_text(true).contains("Fallback body"));
}

#[test]
fn parses_summary_and_body() {
    let headers = read_header_block(Cursor::new(MAIL)).unwrap();
    let summary = parse_summary(
        MessageId("fixture".into()),
        &headers,
        MAIL.len() as u64,
        MessageFlags::empty(),
    )
    .unwrap();
    assert_eq!(summary.subject, "[PATCH v2 1/2] usb: fix thing");
    assert_eq!(summary.from_address, "person@example.com");
    assert!(summary.unread());

    let body = parse_body(MessageId("fixture".into()), MAIL).unwrap();
    assert!(body.display_text(false).contains("Hello from the body"));
}

#[test]
fn renders_html_only_mail_without_external_filters() {
    let raw = b"From: Example <e@example.com>\r\n\
Subject: HTML\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<table><tr><td>Hello</td><td>World</td></tr></table>";
    let body = parse_body(MessageId("html".into()), raw).unwrap();
    assert!(
        body.html_text
            .as_deref()
            .is_some_and(|text| text.contains("Hello"))
    );
    assert!(body.display_text(false).contains("Hello"));
}
