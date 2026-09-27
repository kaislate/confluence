# Confluence

Free, open-source, low-latency audio matrix router for Windows. No nag screens, no timers, no lockouts — ever.

Status: Milestone 0 in progress. Done so far:

- the engine core and clock-drift correction;
- the headless engine;
- ASIO (several drivers in one process), WASAPI and per-application capture;
- VASIO, a virtual ASIO driver that connects DAWs to the engine.

## Build and test

    cargo build --workspace   # also builds the VASIO DLL that one test loads
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

## VASIO (virtual ASIO for DAWs)

`confluence_vasio.dll` gives DAWs eight ASIO drivers, "Confluence VASIO 1" to "8". Register it once from an **administrator** terminal (`regsvr32 /u` removes it):

    cargo build --release -p confluence-vasio
    regsvr32 "<full path>\confluence_vasio.dll"

Then serve an instance from the engine and route to and from it like any other device:

    cargo run -p confluence-cli -- add-device vasio 1         # 2 in / 2 out
    cargo run -p confluence-cli -- add-device vasio 2:8x2     # 8 DAW inputs, 2 DAW outputs

VASIO runs on the engine's clock at the engine's sample rate and block size (the DAW cannot change them), and adds two blocks of round-trip latency. If the engine is not running, the DAW keeps running on silence and reconnects by itself when the engine starts.

## Hardware tests

Plain `cargo test` and CI never open real devices. Opt-in tests on your own hardware output silence only:

    CONFLUENCE_HW_ASIO="GoXLR ASIO Driver" cargo test -p confluence-provider-asio --test hardware -- --ignored --nocapture
    CONFLUENCE_HW_WASAPI=1 cargo test -p confluence-provider-wasapi --test hardware -- --ignored --nocapture
    CONFLUENCE_HW_MASTER="GoXLR ASIO Driver" CONFLUENCE_HW_ASIO_SOFT="VB-Matrix VASIO-8" \
      CONFLUENCE_HW_CAPTURE="Microphone (HD Pro Webcam C920)" CONFLUENCE_HW_SOAK_SECONDS=120 \
      cargo test --release -p confluence-engine --test hardware_soak -- --ignored --nocapture

Licensed under GPL-3.0-or-later.
