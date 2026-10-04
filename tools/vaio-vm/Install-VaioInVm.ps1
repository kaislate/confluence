# SPDX-License-Identifier: MIT
# Copies the signed driver package into the test VM and (re)installs it as
# ROOT\ConfluenceVaio, with Driver Verifier on. Run build.ps1 and sign.ps1 first.
param([switch] $NoVerifier)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'VaioVm.psm1') -Force
$driver = Join-Path $PSScriptRoot '..\..\drivers\vaio'
$pkg = Join-Path $driver 'out\package'
$cer = Join-Path $driver 'out\ConfluenceVaioTest.cer'
$devcon = Get-ChildItem (Join-Path $driver 'packages') -Recurse -Filter devcon.exe | Where-Object FullName -match '\\x64\\' | Select-Object -First 1
if (-not $devcon) { throw "devcon.exe not found in the NuGet WDK" }
$files = @((Get-ChildItem $pkg).FullName) + $cer + $devcon.FullName
Copy-ToVaioVm -Path $files -Destination 'C:\vaio'

$result = Invoke-VaioVm -ArgumentList (-not $NoVerifier) -ScriptBlock {
    param($verifier)
    $ErrorActionPreference = 'Stop'
    certutil -f -addstore Root C:\vaio\ConfluenceVaioTest.cer | Out-Null
    certutil -f -addstore TrustedPublisher C:\vaio\ConfluenceVaioTest.cer | Out-Null
    & C:\vaio\devcon.exe remove 'ROOT\ConfluenceVaio' | Out-Null
    Get-WindowsDriver -Online | Where-Object OriginalFileName -like '*confluencevaio.inf' |
        ForEach-Object { pnputil /delete-driver $_.Driver /uninstall /force | Out-Null }
    $out = & C:\vaio\devcon.exe install C:\vaio\ConfluenceVaio.inf 'ROOT\ConfluenceVaio' 2>&1
    $needsReboot = $false
    if ($verifier) {
        $v = verifier /querysettings | Out-String
        if ($v -notmatch 'ConfluenceVaio.sys') { verifier /standard /driver ConfluenceVaio.sys | Out-Null; $needsReboot = $true }
    }
    [pscustomobject]@{ Devcon = ($out -join "`n"); NeedsReboot = $needsReboot }
}
$result.Devcon
if ($result.NeedsReboot) {
    Restart-VaioVm
}
Invoke-VaioVm {
    [pscustomobject]@{
        Endpoint = (Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue | Where-Object FriendlyName -like '*Confluence VAIO*').FriendlyName
        Device   = (Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue | Where-Object FriendlyName -eq 'Confluence VAIO').Status
        Verifier = ((verifier /querysettings | Out-String) -match 'ConfluenceVaio.sys')
        Control  = (Test-Path '\\.\ConfluenceVaio')
    }
}
