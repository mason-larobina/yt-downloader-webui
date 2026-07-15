#!/usr/bin/env bash
set -xe
cargo fmt
# Format only this repo's own markdown; the vendored yt-dlp/ clone is
# gitignored and must not be touched.
mdformat README.md docs/*.md tests/*.md --wrap no
