# SPDX-License-Identifier: MIT
# Saves a PNG of the test VM's console (to see setup or a bugcheck screen).
param([string] $Path = (Join-Path $env:TEMP 'vaio-vm-screen.png'), [int] $Width = 800, [int] $Height = 600)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'VaioVm.psm1') -Force
$name = Get-VaioVmName
$vmms = Get-CimInstance -Namespace root\virtualization\v2 -ClassName Msvm_VirtualSystemManagementService
$vs = Get-CimInstance -Namespace root\virtualization\v2 -ClassName Msvm_ComputerSystem -Filter "ElementName='$name'" |
    Get-CimAssociatedInstance -ResultClassName Msvm_VirtualSystemSettingData |
    Where-Object VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized'
$r = Invoke-CimMethod -InputObject $vmms -MethodName GetVirtualSystemThumbnailImage `
    -Arguments @{ TargetSystem = $vs; WidthPixels = [uint16]$Width; HeightPixels = [uint16]$Height }
$img = [byte[]]$r.ImageData
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap $Width, $Height, ([System.Drawing.Imaging.PixelFormat]::Format16bppRgb565)
$data = $bmp.LockBits((New-Object System.Drawing.Rectangle 0, 0, $Width, $Height), 'WriteOnly', $bmp.PixelFormat)
try {
    # 16-bit RGB565 rows, tightly packed; the bitmap's own stride may be padded.
    for ($y = 0; $y -lt $Height; $y++) {
        $src = $y * $Width * 2
        if ($src + $Width * 2 -gt $img.Length) { break }
        [System.Runtime.InteropServices.Marshal]::Copy($img, $src, [IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $Width * 2)
    }
} finally { $bmp.UnlockBits($data) }
$bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
$Path
