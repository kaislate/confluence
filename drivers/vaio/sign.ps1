# SPDX-License-Identifier: MIT
# Test-signs the built driver: a self-signed code-signing cert (created once,
# kept in the user's store), a catalog, and signatures on the .sys and .cat.
# Only for machines with test signing on (the VAIO test VM).
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$out = Join-Path $root 'out'
$pkg = Join-Path $out 'package'
$subject = 'CN=Confluence VAIO Test'

$cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object Subject -eq $subject | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -CertStoreLocation Cert:\CurrentUser\My -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(5)
}
Export-Certificate -Cert $cert -FilePath (Join-Path $out 'ConfluenceVaioTest.cer') | Out-Null

function Find-Tool($name, $arch) {
    $t = Get-ChildItem (Join-Path $root 'packages') -Recurse -Filter $name |
        Where-Object FullName -match "\\$arch\\" | Select-Object -First 1
    if (-not $t) { throw "$name ($arch) not found under drivers\vaio\packages; run build.ps1 first" }
    $t.FullName
}
$signtool = Find-Tool 'signtool.exe' 'x64'
$inf2cat = Find-Tool 'Inf2Cat.exe' 'x86'

Remove-Item -Recurse -Force $pkg -ErrorAction SilentlyContinue
New-Item -ItemType Directory $pkg | Out-Null
Copy-Item (Join-Path $out 'ConfluenceVaio.sys'), (Join-Path $out 'ConfluenceVaio.inf') $pkg

& $signtool sign /q /fd sha256 /sha1 $cert.Thumbprint (Join-Path $pkg 'ConfluenceVaio.sys')
if ($LASTEXITCODE) { throw "signing the .sys failed" }
& $inf2cat /driver:$pkg /os:10_X64,10_GE_X64,10_25H2_X64 /uselocaltime
if ($LASTEXITCODE) { throw "Inf2Cat failed" }
& $signtool sign /q /fd sha256 /sha1 $cert.Thumbprint (Join-Path $pkg 'ConfluenceVaio.cat')
if ($LASTEXITCODE) { throw "signing the .cat failed" }
Get-ChildItem $pkg
