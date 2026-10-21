//! `read_file` returns images and PDFs as inline data, sniffed from their bytes, under the
//! same confinement as text (spec 3.1 §10 D6, §9.2 I55).

mod common;

use std::sync::Arc;

use adk_core::{FunctionResponseData, Tool};
use adk_devtools::{DEFAULT_MAX_INLINE_BYTES, ReadFileTool, Workspace};
use common::TestCtx;
use serde_json::{Value, json};

/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0xE2, 0x26, 0x05, 0x9B, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0xFF, 0xD9];
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj << >> endobj\n%%EOF\n";

async fn read(tool: &ReadFileTool, path: &str) -> adk_core::Result<Value> {
    tool.execute(Arc::new(TestCtx), json!({ "path": path })).await
}

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), PNG).unwrap();
    std::fs::write(dir.path().join("photo.jpg"), JPEG).unwrap();
    std::fs::write(dir.path().join("spec.pdf"), PDF).unwrap();
    std::fs::write(dir.path().join("fake.png"), "not an image\nsecond line\n").unwrap();
    std::fs::write(dir.path().join("disguised.txt"), PNG).unwrap();
    std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150, 255]).unwrap();
    let workspace = Workspace::new(dir.path());
    (dir, workspace)
}

#[tokio::test]
async fn png_jpeg_and_pdf_come_back_as_inline_data_with_a_stub() {
    let (_dir, workspace) = fixture();
    let tool = ReadFileTool::new(workspace);
    for (path, mime, bytes) in [
        ("shot.png", "image/png", PNG),
        ("photo.jpg", "image/jpeg", JPEG),
        ("spec.pdf", "application/pdf", PDF),
    ] {
        let value = read(&tool, path).await.unwrap();
        let response = FunctionResponseData::from_tool_result("read_file", value.clone());
        assert_eq!(response.inline_data.len(), 1, "{path}: {value}");
        assert_eq!(response.inline_data[0].mime_type, mime);
        assert_eq!(response.inline_data[0].data, bytes);
        assert_eq!(response.response["mime_type"], mime);
        assert_eq!(response.response["bytes"], bytes.len());
        assert_eq!(response.response["inline"], true);
        let stub = response.response["content"].as_str().unwrap();
        assert!(stub.contains(mime) && stub.contains(&bytes.len().to_string()), "{stub}");
        assert!(
            response.response.get("total_lines").is_none(),
            "no text fields for binary content"
        );
    }
}

#[tokio::test]
async fn the_type_comes_from_the_bytes_not_the_extension() {
    let (_dir, workspace) = fixture();
    let tool = ReadFileTool::new(workspace);
    let text = read(&tool, "fake.png").await.unwrap();
    assert!(text.get("inline_data").is_none(), "{text}");
    assert_eq!(text["total_lines"], 2);
    assert!(text["content"].as_str().unwrap().contains("not an image"));

    let image = read(&tool, "disguised.txt").await.unwrap();
    let response = FunctionResponseData::from_tool_result("read_file", image);
    assert_eq!(response.inline_data[0].mime_type, "image/png");
}

#[tokio::test]
async fn other_binary_content_is_refused_as_before() {
    let (_dir, workspace) = fixture();
    let tool = ReadFileTool::new(workspace);
    let error = read(&tool, "blob.bin").await.unwrap_err();
    assert!(error.message.contains("neither UTF-8 text nor"), "{error}");
}

#[tokio::test]
async fn the_cap_and_text_only_return_the_stub_without_bytes() {
    let (_dir, workspace) = fixture();
    let capped = ReadFileTool::new(workspace.clone()).max_inline_bytes(PNG.len() - 1);
    let value = read(&capped, "shot.png").await.unwrap();
    assert!(value.get("inline_data").is_none(), "{value}");
    assert_eq!(value["inline"], false);
    assert_eq!(value["mime_type"], "image/png");
    assert!(value["content"].as_str().unwrap().contains("inline cap"));

    let text_only = ReadFileTool::new(workspace).text_only();
    let value = read(&text_only, "spec.pdf").await.unwrap();
    assert!(value.get("inline_data").is_none(), "{value}");
    assert_eq!(value["mime_type"], "application/pdf");
    assert!(value["content"].as_str().unwrap().contains("no binary data"));

    assert_eq!(DEFAULT_MAX_INLINE_BYTES, 4 * 1024 * 1024);
    let huge = ReadFileTool::new(Workspace::new(".")).max_inline_bytes(usize::MAX);
    let _ = huge;
}

#[tokio::test]
async fn binary_reads_keep_exclusion_and_containment() {
    let (dir, workspace) = fixture();
    std::fs::write(dir.path().join("secret.pem"), PNG).unwrap();
    let tool = ReadFileTool::new(workspace);
    let error = read(&tool, "secret.pem").await.unwrap_err();
    assert_eq!(error.code, "devtools.path_excluded", "{error}");
    let error = read(&tool, "../outside.png").await.unwrap_err();
    assert!(error.message.contains("outside") || error.message.contains("escape"), "{error}");
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("host.png"), PNG).unwrap();
        std::os::unix::fs::symlink(outside.path().join("host.png"), dir.path().join("link.png"))
            .unwrap();
        assert!(read(&tool, "link.png").await.is_err(), "a link out of the workspace is refused");
        std::fs::hard_link(dir.path().join("shot.png"), dir.path().join("twin.png")).unwrap();
        let error = read(&tool, "twin.png").await.unwrap_err();
        assert!(error.message.contains("hard links"), "{error}");
    }
}
