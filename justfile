# Run the desktop app with frontend HMR (starts Vite, then tauri dev)
app:
    deno task --config frontend/deno.json app

# Build everything
build:
    cargo build

# Build the standalone debug application and its sibling worker.
build-app:
    cargo build -p frances-worker
    deno task --config frontend/deno.json tauri build --debug --no-bundle

# Build the standalone release application and its sibling worker.
bundle:
    cargo build --release -p frances-worker
    deno task --config frontend/deno.json tauri build --no-bundle

# Run the already-built standalone release binary.
bundle-run *args:
    exec ./target/release/frances {{ args }}

# Build an AppImage containing the musl worker for this Linux host.
appimage:
    #!/usr/bin/env bash
    set -euo pipefail
    source opt/lib/linux-targets.sh
    frances_detect_linux_targets

    if command -v podman >/dev/null; then
        container_runtime=podman
        container_user=()
    elif command -v docker >/dev/null; then
        container_runtime=docker
        container_user=(--user "$(id -u):$(id -g)")
    else
        echo "building an AppImage requires podman or docker" >&2
        exit 1
    fi

    "$container_runtime" build \
        --tag frances-appimage-builder \
        --file opt/appimage.Containerfile \
        opt

    repo_dir=$(pwd)
    exec "$container_runtime" run --rm \
        "${container_user[@]}" \
        --volume "$repo_dir:/workspace" \
        --workdir /workspace \
        --env CARGO_HOME=/workspace/target/appimage-cache/cargo \
        --env DENO_DIR=/workspace/target/appimage-cache/deno \
        --env XDG_CACHE_HOME=/workspace/target/appimage-cache/xdg \
        frances-appimage-builder \
        bash -c '
            set -euo pipefail
            worker_target=$1
            cargo build --release --locked -p frances-worker --target "$worker_target"
            gzip -9 -c "target/$worker_target/release/frances-worker" \
                > "target/$worker_target/release/frances-worker.gz"
            exec ./opt/bin/build-appimage \
                --worker-image "$worker_target=target/$worker_target/release/frances-worker.gz"
        ' bash "$FRANCES_MUSL_TARGET"

# Run the already-built AppImage for this Linux host.
appimage-run:
    #!/usr/bin/env bash
    set -euo pipefail
    source opt/lib/linux-targets.sh
    frances_detect_linux_targets

    bundle_dir="target/$FRANCES_GNU_TARGET/release/bundle/appimage"
    mapfile -t appimages < <(find "$bundle_dir" -maxdepth 1 -type f -name '*.AppImage' -print 2>/dev/null)
    if ((${#appimages[@]} != 1)); then
        echo "expected one built AppImage in $bundle_dir, found ${#appimages[@]}" >&2
        exit 1
    fi

    if command -v appimage-run >/dev/null; then
        exec appimage-run "${appimages[0]}" --foreground
    fi
    exec "${appimages[0]}" --foreground

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
