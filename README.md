# Confluence

Free, open-source, low-latency audio matrix router for Windows. No nag screens, no timers, no lockouts — ever.

Status: Milestone 0 in progress — engine core, clock-drift correction and headless engine.

## Build and test

    cargo test --workspace
    cargo run -p confluence-engine
    cargo run -p confluence-cli -- health

Licensed under GPL-3.0-or-later.
