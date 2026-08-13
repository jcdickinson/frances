# Run the desktop app with frontend HMR (starts Vite, then tauri dev)
app:
    deno task --config frontend/deno.json app

# Build everything
build:
    cargo build

# Build the frontend, then the binary (standalone run, no dev server)
build-app:
    cd frontend && deno task build
    cargo build -p frances

# Type-check the frontend
check:
    cd frontend && deno task check

# Run all tests, or one crate: just test -p frances-edit
test *args:
    cargo nextest run {{ args }}

fmt:
    cargo fmt --all
    cd frontend && deno task fmt

lint:
    cargo clippy --all-targets
    cd frontend && deno task lint

# Find unused crate dependencies
machete:
    cargo machete
