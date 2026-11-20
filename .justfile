# Build optimized rvt and ctx binaries.
build:
    cargo build --release

# Run the test suite.
test:
    cargo test

# Build rvt and ctx and copy them to ~/bin.
install: build
    mkdir -p ~/bin
    install -m 755 target/release/rvt ~/bin/rvt
    install -m 755 target/release/ctx ~/bin/ctx
