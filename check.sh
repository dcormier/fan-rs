cargo clippy && \
cargo clippy --all-features && \
cargo clippy --no-default-features --features tokio && \
cargo clippy --tests && \
cargo clippy --tests --all-features && \
cargo clippy --tests --no-default-features --features tokio && \
cargo test && \
cargo test --all-features && \
cargo test --no-default-features --features tokio && \
cargo doc --all-features
