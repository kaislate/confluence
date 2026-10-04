# SPDX-License-Identifier: MIT
# Builds the VAIO VM tests on the host, copies the test binary into the VM and
# runs it there (the driver must be installed: Install-VaioInVm.ps1).
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'VaioVm.psm1') -Force
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
# A clean Windows has no Visual C++ runtime: link the CRT statically, in a
# separate target directory so the normal build cache is not invalidated.
$saved = $env:RUSTFLAGS, $env:CARGO_TARGET_DIR
try {
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    $env:CARGO_TARGET_DIR = Join-Path $repo 'target\vm'
    $json = cargo test --release -p confluence-provider-vaio --test vm --no-run --message-format=json --manifest-path (Join-Path $repo 'Cargo.toml')
} finally {
    # Leave the caller's session as it was (later cargo runs use the normal build).
    $env:RUSTFLAGS, $env:CARGO_TARGET_DIR = $saved
}
$exe = ($json | ForEach-Object { $_ | ConvertFrom-Json -ErrorAction SilentlyContinue } |
    Where-Object { $_.reason -eq 'compiler-artifact' -and $_.executable } | Select-Object -Last 1).executable
if (-not $exe) { throw "no test executable" }
Copy-ToVaioVm -Path $exe -Destination 'C:\vaio'
$name = Split-Path $exe -Leaf
Invoke-VaioVm -ArgumentList $name -ScriptBlock {
    param($name)
    $env:CONFLUENCE_VM_VAIO = '1'
    & "C:\vaio\$name" --ignored --test-threads=1 2>&1
    "exit $LASTEXITCODE"
}
