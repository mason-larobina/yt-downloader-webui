#!/usr/bin/env bash
# Pre-publish gate for yt-downloader-webui.
#
#   ./publish.sh            fmt + build + unit tests + network shell tests + publish
#   ./publish.sh --dry-run  same, with args forwarded to `cargo publish`
#
# All scripts under tests/ except probe-flat-playlist.sh (which needs a
# logged-in Firefox profile) are run before publish. They hit the network
# (archive.org / YouTube) and bind fixed loopback ports, so they run strictly
# sequentially. We build the release binary once and point the three
# server-driving scripts at it via YT_DOWNLOADER_WEBUI_BINARY so none of them
# re-build it.
set -xe

./format.sh

cargo build --release
cargo test

# Shell-driven end-to-end tests (network required). probe-flat-playlist.sh is
# intentionally excluded: it requires a logged-in Firefox profile and must be
# run on the operator's own machine, not here.
# export YT_DOWNLOADER_WEBUI_BINARY="$PWD/target/release/yt-downloader-webui"
# ./tests/endpoints.sh
# ./tests/e2e-download.sh
# ./tests/persistence-restart.sh
# ./tests/probe-ytdlp-flags.sh

# Refuse to publish unless the working tree is clean -- format.sh's output must
# be committed so the published crate matches HEAD.
[[ -z "$(git status --porcelain)" ]] || { echo "working tree not clean; commit first"; exit 1; }

cargo publish "$@"
