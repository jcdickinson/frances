frances_detect_linux_targets() {
    local architecture=${1:-$(uname -m)}
    case "$architecture" in
        x86_64 | x86_64-*)
            FRANCES_GNU_TARGET=x86_64-unknown-linux-gnu
            FRANCES_MUSL_TARGET=x86_64-unknown-linux-musl
            FRANCES_APPIMAGE_ARCH=x86_64
            ;;
        aarch64 | aarch64-* | arm64)
            FRANCES_GNU_TARGET=aarch64-unknown-linux-gnu
            FRANCES_MUSL_TARGET=aarch64-unknown-linux-musl
            FRANCES_APPIMAGE_ARCH=aarch64
            ;;
        *)
            echo "unsupported Linux architecture: $architecture" >&2
            return 1
            ;;
    esac
}
