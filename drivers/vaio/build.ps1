# SPDX-License-Identifier: MIT
# Builds the Confluence VAIO driver with the NuGet WDK into drivers\vaio\out.
param([ValidateSet('Release', 'Debug')] [string] $Configuration = 'Release')
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$tools = Join-Path $root '.tools'
$nuget = Join-Path $tools 'nuget.exe'
if (-not (Test-Path $nuget)) {
    New-Item -ItemType Directory -Force $tools | Out-Null
    Invoke-WebRequest https://dist.nuget.org/win-x86-commandline/latest/nuget.exe -OutFile $nuget
}
& $nuget restore (Join-Path $root 'packages.config') -PackagesDirectory (Join-Path $root 'packages') -NonInteractive `
    -Source https://api.nuget.org/v3/index.json
if ($LASTEXITCODE) { throw "nuget restore failed" }

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$msbuild = & $vswhere -nologo -products * -latest -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\amd64\MSBuild.exe' | Select-Object -First 1
if (-not $msbuild) { throw "MSBuild not found (Visual Studio Build Tools 2022)" }

# The 64-bit MSBuild: the WDK then picks its x64 tools (the NuGet WDK ships no x86 stampinf).
# Utilities and Filters are static libraries that Main links by path.
foreach ($proj in 'Utilities\Utilities.vcxproj', 'Filters\Filters.vcxproj', 'Main\Main.vcxproj') {
    & $msbuild (Join-Path $root "Source\$proj") /nologo /m /v:minimal `
        "/p:Configuration=$Configuration" /p:Platform=x64 /p:SignMode=Off
    if ($LASTEXITCODE) { throw "msbuild $proj failed" }
}

$out = Join-Path $root 'out'
New-Item -ItemType Directory -Force $out | Out-Null
$built = Join-Path $root "Source\Main\x64\$Configuration"
Copy-Item (Join-Path $built 'ConfluenceVaio.sys'), (Join-Path $built 'ConfluenceVaio.inf') $out -Force
Get-ChildItem $out
