# Confluence

Free, open-source, low-latency audio matrix router for Windows. No nag screens, no timers, no lockouts — ever.

Status: Milestone 0 in progress. Done so far:

- the engine core and clock-drift correction;
- the headless engine;
- ASIO (several drivers in one process), WASAPI and per-application capture.

## Build and test

    cargo test --workspace
    cargo run -p confluence-engine
    cargo run -p confluence-cli -- health

## Devices

Run the engine on an ASIO device's clock, or on the internal clock (the default):

    cargo run --release -p confluence-engine -- --master "asio:GoXLR ASIO Driver"

Add devices as soft slots. Each one gets its own channels and a drift-corrected bridge to the master. Devices are saved in `%LOCALAPPDATA%\Confluence\devices.json` and come back, on the same channels, at the next start.

    cargo run -p confluence-cli -- devices
    cargo run -p confluence-cli -- add-device asio "VB-Matrix VASIO-8"
    cargo run -p confluence-cli -- add-device wasapi-out "Speakers (Realtek(R) Audio)"
    cargo run -p confluence-cli -- add-device wasapi-in "Microphone (HD Pro Webcam C920)"
    cargo run -p confluence-cli -- add-device app Discord
    cargo run -p confluence-cli -- slots
    cargo run -p confluence-cli -- remove-slot 3

A saved device that is missing at startup keeps its channels as an OFFLINE slot, so routes to it survive.

## Hardware tests

Plain `cargo test` and CI never open real devices. Opt-in tests on your own hardware output silence only:

    CONFLUENCE_HW_ASIO="GoXLR ASIO Driver" cargo test -p confluence-provider-asio --test hardware -- --ignored --nocapture
    CONFLUENCE_HW_WASAPI=1 cargo test -p confluence-provider-wasapi --test hardware -- --ignored --nocapture
    CONFLUENCE_HW_MASTER="GoXLR ASIO Driver" CONFLUENCE_HW_ASIO_SOFT="VB-Matrix VASIO-8" \
      CONFLUENCE_HW_CAPTURE="Microphone (HD Pro Webcam C920)" CONFLUENCE_HW_SOAK_SECONDS=120 \
      cargo test --release -p confluence-engine --test hardware_soak -- --ignored --nocapture

Licensed under GPL-3.0-or-later.
