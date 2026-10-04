# SPDX-License-Identifier: MIT
# Builds and runs the user-mode pump tests with the Build Tools' cl.exe.
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -products * -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$env:PATH = "$(Split-Path $vswhere);$env:PATH"   # vcvars64.bat calls vswhere
$cmd = "`"$vcvars`" >nul && cd /d `"$here`" && cl /nologo /EHsc /W4 /WX /std:c++17 pump_test.cpp /Fe:pump_test.exe && .\pump_test.exe"
cmd /c $cmd
exit $LASTEXITCODE
