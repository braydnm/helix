#!/bin/sh

RUSTFLAGS="-C target-cpu=native" cargo install --path helix-term --locked -F native
hx --grammar fetch
hx --grammar build
