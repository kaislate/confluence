# SPDX-License-Identifier: MIT
# Helpers for the Confluence VAIO test VM (PowerShell Direct; no network needed).
$script:VmName = 'ConfluenceVaioTest'
$script:VmRoot = Join-Path $env:LOCALAPPDATA 'Confluence\vaio-vm'
$script:CredPath = Join-Path $script:VmRoot 'tester.credential.xml'

function Get-VaioVmName { $script:VmName }
function Get-VaioVmRoot { $script:VmRoot }

# The guest admin's credential, saved (DPAPI, this user only) when the VM was created.
function Get-VaioVmCredential {
    if (-not (Test-Path $script:CredPath)) { throw "No VM credential at $script:CredPath; run New-VaioTestVm.ps1" }
    Import-Clixml $script:CredPath
}

function Invoke-VaioVm {
    param([Parameter(Mandatory)] [scriptblock] $ScriptBlock, [object[]] $ArgumentList = @())
    Invoke-Command -VMName $script:VmName -Credential (Get-VaioVmCredential) -ScriptBlock $ScriptBlock -ArgumentList $ArgumentList
}

function Copy-ToVaioVm {
    param([Parameter(Mandatory)] [string[]] $Path, [Parameter(Mandatory)] [string] $Destination)
    $s = New-PSSession -VMName $script:VmName -Credential (Get-VaioVmCredential)
    try {
        Invoke-Command -Session $s { param($d) New-Item -ItemType Directory -Force $d | Out-Null } -ArgumentList $Destination
        Copy-Item -ToSession $s -Path $Path -Destination $Destination -Recurse -Force
    } finally { Remove-PSSession $s }
}

# Waits until PowerShell Direct answers (after a boot or a crash).
function Wait-VaioVm([int] $TimeoutSeconds = 600) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        try { if (Invoke-VaioVm { $true } -ErrorAction Stop) { return } } catch { Start-Sleep 5 }
    }
    throw "The VM did not answer within $TimeoutSeconds s"
}

# A clean reboot from inside the guest, waiting until it has really booted
# again. (Restart-VM defaults to a hard reset, which can lose registry writes
# such as Driver Verifier settings.)
function Restart-VaioVm([int] $TimeoutSeconds = 600) {
    $before = Invoke-VaioVm { (Get-CimInstance Win32_OperatingSystem).LastBootUpTime }
    Invoke-VaioVm { Restart-Computer -Force }
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep 5
        try {
            $now = Invoke-VaioVm { (Get-CimInstance Win32_OperatingSystem).LastBootUpTime } -ErrorAction Stop
            if ($now -ne $before) { return }
        } catch { }
    }
    throw "The VM did not come back from a reboot within $TimeoutSeconds s"
}

# Back to the state right after setup (e.g. after a driver bugcheck).
function Restore-VaioVm {
    Restore-VMCheckpoint -VMName $script:VmName -Name 'clean' -Confirm:$false
    Start-VM -Name $script:VmName
    Wait-VaioVm
}

Export-ModuleMember -Function Get-VaioVmName, Get-VaioVmRoot, Get-VaioVmCredential, Invoke-VaioVm, Copy-ToVaioVm, Wait-VaioVm, Restart-VaioVm, Restore-VaioVm
