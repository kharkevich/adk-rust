- **`read_file` returns images and PDFs as inline data** (`adk-devtools`, spec 3.1 §10 D6):
  a file whose bytes start like a PNG, JPEG, GIF, WebP or PDF comes back as an
  `inline_data` part with a text stub naming the type and size, so a model with vision sees
  the picture and a text-only provider still learns what the file is. The type comes from
  the bytes, never the extension; the read follows the same confinement, exclusion and
  no-follow rules as text; `ReadFileTool::max_inline_bytes` (default 4 MiB) caps the
  attachment and `ReadFileTool::text_only` returns the stub only. Other binary content is
  refused as before. `examples/harness_agent` gains `read_file_image_inline`.
