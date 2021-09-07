#!/bin/bash
if which cargo; then
    cargo build --release
else
    echo "Cargo was not found, using prebuilt"
fi
