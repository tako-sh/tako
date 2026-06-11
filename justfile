mod blog 'just/blog.just'
mod build 'just/build.just'
mod check 'just/check.just'
mod testbed 'just/testbed.just'
mod test 'just/test.just'

export TAKO_HOME := "local-dev/.tako"

tako *arguments:
    cargo build -p tako-cli --release
    TAKO_HOME="$(pwd)/{{ TAKO_HOME }}" ./target/release/tako {{ arguments }}

fmt:
    cargo fmt
    bun run fmt
    gofmt -w . examples/go/

lint:
    cargo clippy --fix --allow-dirty --workspace --all-targets
    bun run lint
    bun run --filter '*' typecheck
    go vet ./...
    for dir in examples/go/*/; do (cd "$dir" && go vet ./...); done

ci: fmt lint test::all


e2e fixture="e2e/fixtures/javascript/tanstack-start": (test::e2e fixture)

# Bump the published Rust SDK crate version.
sdk-rust part:
    bun scripts/bump-rust-sdk-version.ts {{ part }}
