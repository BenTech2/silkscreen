# Sourced by the packaging scripts. Compilers record source file paths (for
# panic messages, asserts and debug info); this rewrites the builder's home
# folder, and so their user name, to /build in everything they produce.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=/build"
export CFLAGS="${CFLAGS:+$CFLAGS }-ffile-prefix-map=$HOME=/build"
export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-ffile-prefix-map=$HOME=/build"
export OBJCFLAGS="${OBJCFLAGS:+$OBJCFLAGS }-ffile-prefix-map=$HOME=/build"

# Fails if any file under $1 still mentions the home folder.
check_no_home_paths() {
    if grep -rlaF "$HOME" "$1"; then
        echo "error: the files above contain $HOME" >&2
        exit 1
    fi
}
