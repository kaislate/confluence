# SPDX-License-Identifier: MIT
# Creates the Confluence VAIO test VM: Windows 11 installed unattended, Secure
# Boot off (so test signing can be turned on), a checkpoint "clean" at the end.
param(
    [string] $IsoPath = (Join-Path $env:USERPROFILE 'Downloads\Win11_25H2_English_x64_v2.iso'),
    [int] $Cpus = 4,
    [long] $MemoryBytes = 8GB,
    [long] $DiskBytes = 80GB,
    # Skip creation: only wait for an install that is already running, then finish setup.
    [switch] $Resume
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'VaioVm.psm1') -Force
$name = Get-VaioVmName
$root = Get-VaioVmRoot
if (-not $Resume) {
if (Get-VM -Name $name -ErrorAction SilentlyContinue) { throw "VM $name already exists" }
if (-not (Test-Path $IsoPath)) { throw "Windows ISO not found: $IsoPath" }
New-Item -ItemType Directory -Force $root | Out-Null

# A random password for the guest admin, kept only in a DPAPI-protected file.
$chars = [char[]]'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'
$password = -join (1..16 | ForEach-Object { $chars | Get-Random }) + '!7'
$cred = New-Object PSCredential ('tester', (ConvertTo-SecureString $password -AsPlainText -Force))
$cred | Export-Clixml (Join-Path $root 'tester.credential.xml')

# The answer file on its own small ISO (Windows Setup reads autounattend.xml from any drive's root).
$stage = Join-Path $root 'unattend'
New-Item -ItemType Directory -Force $stage | Out-Null
(Get-Content (Join-Path $PSScriptRoot 'autounattend.template.xml') -Raw).Replace('@@PASSWORD@@', $password) |
    Set-Content -Encoding utf8 (Join-Path $stage 'autounattend.xml')
if (-not ('VaioIsoWriter' -as [type])) {
    Add-Type -TypeDefinition @'
using System; using System.IO; using System.Runtime.InteropServices; using System.Runtime.InteropServices.ComTypes;
public static class VaioIsoWriter {
  public static void Write(object comStream, string path) {
    IStream s = (IStream)comStream; byte[] buf = new byte[65536]; IntPtr read = Marshal.AllocHGlobal(4);
    try { using (FileStream f = File.Create(path)) {
      while (true) { s.Read(buf, buf.Length, read); int n = Marshal.ReadInt32(read); if (n <= 0) break; f.Write(buf, 0, n); } } }
    finally { Marshal.FreeHGlobal(read); }
  }
}
'@
}
$fs = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
$fs.FileSystemsToCreate = 3
$fs.VolumeName = 'UNATTEND'
$fs.Root.AddTree($stage, $false)
$answerIso = Join-Path $root 'autounattend.iso'
[VaioIsoWriter]::Write($fs.CreateResultImage().ImageStream, $answerIso)
Remove-Item -Recurse -Force $stage

New-VM -Name $name -Generation 2 -MemoryStartupBytes $MemoryBytes -Path $root `
    -NewVHDPath (Join-Path $root "$name.vhdx") -NewVHDSizeBytes $DiskBytes -SwitchName 'Default Switch' | Out-Null
Set-VMProcessor -VMName $name -Count $Cpus
Set-VMMemory -VMName $name -DynamicMemoryEnabled $false
Set-VMFirmware -VMName $name -EnableSecureBoot Off
Set-VM -Name $name -AutomaticCheckpointsEnabled $false -CheckpointType Standard
Enable-VMIntegrationService -VMName $name -Name 'Guest Service Interface'
$dvd = Add-VMDvdDrive -VMName $name -Path $IsoPath -Passthru
Add-VMDvdDrive -VMName $name -Path $answerIso
Set-VMFirmware -VMName $name -FirstBootDevice $dvd

Start-VM -Name $name
# Answer "Press any key to boot from CD or DVD" with Enter. The keyboard is
# looked up on every try (one fetched right after Start-VM may not be live
# yet), and only for 12 s: later keys reach Setup's own Cancel button.
$cs = Get-CimInstance -Namespace root\virtualization\v2 -ClassName Msvm_ComputerSystem -Filter "ElementName='$name'"
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt 12) {
    $kb = Get-CimAssociatedInstance -InputObject $cs -ResultClassName Msvm_Keyboard
    if ($kb) { Invoke-CimMethod -InputObject $kb -MethodName TypeKey -Arguments @{ keyCode = [uint32]0x0D } | Out-Null }
    Start-Sleep -Milliseconds 250
}
} # -not $Resume

Write-Host "Installing Windows in $name (typically 20-40 minutes)..."
$deadline = (Get-Date).AddMinutes(90)
while ($true) {
    if ((Get-Date) -gt $deadline) { throw "Setup did not finish within 90 minutes; check the VM console" }
    try {
        if (Invoke-VaioVm { Test-Path C:\vaio\setup-done.txt } -ErrorAction Stop) { break }
    } catch { }
    Start-Sleep 20
}
# Reboot once so test signing takes effect, then take the clean checkpoint.
Get-VMDvdDrive -VMName $name | Remove-VMDvdDrive
Restart-VaioVm
$ts = Invoke-VaioVm { bcdedit /enum '{current}' | Select-String testsigning }
Write-Host "Guest: $ts"
Stop-VM -Name $name
Checkpoint-VM -Name $name -SnapshotName 'clean'
Start-VM -Name $name
Wait-VaioVm
Write-Host "VM ready. Credential: $(Join-Path $root 'tester.credential.xml')"
