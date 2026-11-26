# Build optimized rvt and ctx binaries.
build:
    cargo build --release

# Run the test suite.
test: check-core
    cargo test

# Fail when ctx-core depends on a terminal UI crate, which belongs in the ctx front end.
check-core:
    #!/usr/bin/env bash
    set -euo pipefail
    found=$(cargo tree -p ctx-core -e normal --prefix none | grep -E '^(ratatui|crossterm|view) ' | sort -u || true)
    if [ -n "$found" ]; then
      echo "ctx-core must not depend on terminal UI crates, but depends on:" >&2
      echo "$found" >&2
      exit 1
    fi

# Build rvt and ctx and copy them to ~/bin.
install: build
    mkdir -p ~/bin
    install -m 755 target/release/rvt ~/bin/rvt
    install -m 755 target/release/ctx ~/bin/ctx

# Raise the workspace version in Cargo.toml by "major", "minor" or "patch".
bump-version part:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ part }}" in
      major|minor|patch) ;;
      *) echo "part must be major, minor, or patch (got '{{ part }}')" >&2; exit 1 ;;
    esac
    current=$(sed -nE '/^\[workspace\.package\]/,/^\[/ s/^version = "([0-9]+\.[0-9]+\.[0-9]+)"/\1/p' Cargo.toml)
    if [ -z "$current" ]; then
      echo "Cargo.toml has no 'version = \"X.Y.Z\"' line under [workspace.package]" >&2
      exit 1
    fi
    IFS=. read -r major minor patch <<< "$current"
    case "{{ part }}" in
      major) major=$((major + 1)); minor=0; patch=0 ;;
      minor) minor=$((minor + 1)); patch=0 ;;
      patch) patch=$((patch + 1)) ;;
    esac
    next="$major.$minor.$patch"
    sed -E -i.bak "/^\[workspace\.package\]/,/^\[/ s/^version = \"[0-9.]+\"/version = \"$next\"/" Cargo.toml
    rm Cargo.toml.bak
    cargo update --workspace --offline --quiet
    echo "$current -> $next"

# Tag HEAD as v<version> from Cargo.toml and push the tag.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(sed -nE '/^\[workspace\.package\]/,/^\[/ s/^version = "([0-9]+\.[0-9]+\.[0-9]+)"/\1/p' Cargo.toml)
    if [ -z "$version" ]; then
      echo "Cargo.toml has no 'version = \"X.Y.Z\"' line under [workspace.package]" >&2
      exit 1
    fi
    tag="v$version"
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null \
       || git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
      echo "Tag $tag already exists. Run 'just bump-version <major|minor|patch>' and commit the change first." >&2
      exit 1
    fi
    if ! git diff --quiet HEAD -- Cargo.toml Cargo.lock; then
      echo "Cargo.toml or Cargo.lock has uncommitted changes. Commit the version bump before releasing." >&2
      exit 1
    fi
    git tag -a "$tag" -m "rvt $tag"
    git push origin "$tag"
    echo "Pushed $tag"
