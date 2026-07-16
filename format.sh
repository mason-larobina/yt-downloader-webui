#!/usr/bin/env bash
set -xe
cargo fmt
prettier --write *.md docs/*.md tests/*.md
prettier --write --print-width=200 static/*.html templates/*.html static/*.css
