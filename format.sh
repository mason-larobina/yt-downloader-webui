#!/usr/bin/env bash
set -xe
cargo fmt
# Format only this repo's own CSS and markdown; the vendored yt-dlp/ clone is
# gitignored and must not be touched. Prettier's default proseWrap=preserve
# keeps md paragraphs unwrapped (equivalent to the old `mdformat --wrap no`).
prettier --write README.md docs/*.md tests/*.md static/*.css
