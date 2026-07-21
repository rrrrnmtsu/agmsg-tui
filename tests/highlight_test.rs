//! Confirms the room pane's syntect wrapper actually tokenizes fenced code
//! (not just wraps the whole block in one flat, unstyled span).
use agmsg_tui::highlight::highlight_body;

#[test]
fn rust_fence_produces_multiple_styled_tokens_and_preserves_surrounding_text() {
    let body = "check this out:\n```rust\nfn main() {\n    let x = 1;\n}\n```\nthanks";
    let lines = highlight_body(body);

    let rendered: String = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("check this out:"));
    assert!(rendered.contains("fn main()"));
    assert!(rendered.contains("thanks"));

    let fn_line = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("fn")))
        .expect("the `fn main()` line survived highlighting");
    assert!(
        fn_line.spans.len() > 1,
        "expected syntect to split the line into multiple styled tokens, got {:?}",
        fn_line.spans
    );
}
