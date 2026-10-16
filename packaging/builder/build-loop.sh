#!/bin/sh
# ArtCraft Launcher build service (cli-builder): builds a Craft App's MCP command-line tool from
# its source when the launcher asks, for apps whose release's tool can't be confined to a work
# folder yet. It sees only the shared builds folder, never the launcher's data.
#
#   $BUILDS/builder.alive          heartbeat (Unix seconds), every 20 s
#   $BUILDS/requests/<app>         a request from the launcher (the file's name is all it says)
#   $BUILDS/status/<app>.json      {"state":"building|done|failed","commit":"<sha>"}
#   $BUILDS/status/<app>.log       the build's output
#   $BUILDS/out/<app>/<sha>/<app>-cli
#
# Only https://github.com/storytold/<app> is ever cloned: the address is built here from the app's
# name, which must be lowercase letters ending in "craft".
set -u
BUILDS="${ARTCRAFT_BUILDS_DIR:-/builds}"
JOBS="${ARTCRAFT_BUILD_JOBS:-2}"
export CARGO_HOME="$BUILDS/cargo"
mkdir -p "$BUILDS/requests" "$BUILDS/status" "$BUILDS/out" "$BUILDS/work" "$CARGO_HOME"

# The heartbeat goes on while a build runs (builds take minutes).
( while :; do date +%s > "$BUILDS/builder.alive.tmp" && mv "$BUILDS/builder.alive.tmp" "$BUILDS/builder.alive"; sleep 20; done ) &

status() { # state [commit]
    printf '{"state":"%s","commit":"%s","at":%s}\n' "$1" "${2:-}" "$(date +%s)" > "$BUILDS/status/$app.json.tmp"
    mv "$BUILDS/status/$app.json.tmp" "$BUILDS/status/$app.json"
}

build() {
    app="$1"
    src="$BUILDS/work/$app"
    log="$BUILDS/status/$app.log"
    rm -rf "$src" "$BUILDS/work/$app-target"
    status building
    echo "cloning https://github.com/storytold/$app" > "$log"
    if ! GIT_LFS_SKIP_SMUDGE=1 git clone --quiet --depth 1 "https://github.com/storytold/$app" "$src" >> "$log" 2>&1; then
        status failed; return
    fi
    commit=$(git -C "$src" rev-parse HEAD)
    status building "$commit"
    echo "building $app-cli at $commit with $(cargo --version)" >> "$log"
    if ! (cd "$src" && CARGO_TARGET_DIR="$BUILDS/work/$app-target" nice cargo build --release --locked -j "$JOBS" -p "$app-cli") >> "$log" 2>&1; then
        status failed "$commit"; rm -rf "$src" "$BUILDS/work/$app-target"; return
    fi
    mkdir -p "$BUILDS/out/$app/$commit"
    cp "$BUILDS/work/$app-target/release/$app-cli" "$BUILDS/out/$app/$commit/$app-cli.tmp" \
        && mv "$BUILDS/out/$app/$commit/$app-cli.tmp" "$BUILDS/out/$app/$commit/$app-cli"
    # Older builds of this app go; the build folders too (several GB).
    for old in "$BUILDS/out/$app"/*; do [ "$old" = "$BUILDS/out/$app/$commit" ] || rm -rf "$old"; done
    rm -rf "$src" "$BUILDS/work/$app-target"
    echo "done" >> "$log"
    status done "$commit"
}

echo "cli-builder: waiting for requests in $BUILDS/requests"
while :; do
    for req in "$BUILDS"/requests/*; do
        [ -e "$req" ] || continue
        name=$(basename "$req")
        rm -f "$req"
        case "$name" in
            *craft) case "$name" in *[!a-z]*) echo "cli-builder: ignored request $name"; continue ;; esac ;;
            *) echo "cli-builder: ignored request $name"; continue ;;
        esac
        echo "cli-builder: building $name"
        build "$name"
        echo "cli-builder: $name $(sed -n 's/.*"state":"\([a-z]*\)".*/\1/p' "$BUILDS/status/$name.json")"
    done
    sleep 5
done
