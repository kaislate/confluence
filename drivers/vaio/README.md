# Confluence VAIO driver

A virtual Windows playback device ("Confluence VAIO") whose audio goes to the Confluence engine.

This directory is a **separate program** from the GPL engine. It talks to the engine only through a control device (`\.\ConfluenceVaio`) and shared memory, and no code is shared.

- **Licences:**
  - Files derived from Microsoft's `simpleaudiosample` (Windows-driver-samples at `2dc3fd3a0cc84a2933f2194e7ec0871584979071`) keep Microsoft's copyright header and are under the MS-PL (`LICENSE-MS-PL.txt`).
  - Files that start with `SPDX-License-Identifier: MIT` are ours, under the MIT licence (`LICENSE-MIT.txt`).
- **Build:** `pwsh build.ps1`. It needs Visual Studio Build Tools 2022 with the Spectre-mitigated libraries and the Driver Kit build component. The WDK comes from NuGet (`packages.config`).
- **Sign and install:** see `sign.ps1` and `tools/vaio-vm/`. The driver is test-signed and runs only on a machine with test signing on (the throwaway test VM), never on a development PC.
