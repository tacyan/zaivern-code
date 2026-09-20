# Test-only image. Uses the existing Rust CI image family; no model/API key.
FROM rust:1-bookworm
COPY mcp-fixture.rs /fixture.rs
RUN rustc --edition 2021 /fixture.rs -o /usr/local/bin/qwen
CMD ["sleep", "1800"]
