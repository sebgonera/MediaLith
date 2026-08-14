# Capture what MediaLith needs to know about a candidate machine, from Windows.
#
# The companion to capture-hardware.sh, for machines that have not been booted into
# Linux yet. Read-only: it queries and prints, changes nothing.
#
#     powershell -ExecutionPolicy Bypass -File capture-hardware.ps1 > capture.txt
#
# Run as Administrator for the complete picture (Secure Boot state and TPM need it);
# an unelevated capture is still useful.
#
# What Windows cannot tell us: VA-API capabilities, and GuC/HuC firmware status. Those
# are Linux-side and need a live USB. Everything needed to build the kernel config is
# here — PCI IDs for graphics, network, and storage controllers above all, since a
# kernel missing those drivers produces a machine that boots and is unusable.

$ErrorActionPreference = 'Continue'

function Section($title) {
    Write-Output ""
    Write-Output "===== $title ====="
}

# Pull the PCI vendor and device ID out of a Windows PNP device path, which looks like
# PCI\VEN_8086&DEV_3EA0&SUBSYS_...  These are the IDs plexos-gpu matches on.
function Get-PciId($instanceId) {
    if ($instanceId -match 'VEN_([0-9A-Fa-f]{4})&DEV_([0-9A-Fa-f]{4})') {
        $vendor = $Matches[1].ToLower()
        $device = $Matches[2].ToLower()
        return "0x$vendor:0x$device"
    }
    return "(not a PCI device)"
}

function Try-Report($label, $block) {
    try {
        & $block
    } catch {
        Write-Output "($label unavailable: $($_.Exception.Message))"
    }
}

Write-Output "MediaLith hardware capture (Windows)"
Write-Output "generated: $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))"
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Output "elevated: $elevated"

Section "System"
Try-Report "system info" {
    $cs = Get-CimInstance Win32_ComputerSystem
    Write-Output "manufacturer: $($cs.Manufacturer)"
    Write-Output "model:        $($cs.Model)"
    Write-Output "ram:          $([math]::Round($cs.TotalPhysicalMemory / 1GB, 1)) GB"
    $bios = Get-CimInstance Win32_BIOS
    Write-Output "bios:         $($bios.Manufacturer) $($bios.SMBIOSBIOSVersion)"
}

Section "CPU"
Try-Report "CPU info" {
    Get-CimInstance Win32_Processor | ForEach-Object {
        Write-Output "name:     $($_.Name.Trim())"
        Write-Output "cores:    $($_.NumberOfCores) physical / $($_.NumberOfLogicalProcessors) logical"
        Write-Output "arch:     $($_.AddressWidth)-bit"
    }
}

Section "Firmware / UEFI"
# MediaLith requires UEFI (ADR-0003). A machine reporting Legacy here needs its firmware
# switched before it can run MediaLith at all.
Try-Report "firmware type" {
    $fw = $env:firmware_type
    if (-not $fw) {
        $fw = (Get-ComputerInfo -Property BiosFirmwareType -ErrorAction Stop).BiosFirmwareType
    }
    Write-Output "firmware type: $fw"
    if ("$fw" -notmatch 'UEFI|Uefi') {
        Write-Output "WARNING: not booted in UEFI mode. MediaLith requires UEFI."
    }
}
Try-Report "secure boot state" {
    Write-Output "secure boot:   $(Confirm-SecureBootUEFI)"
}
Try-Report "TPM" {
    $tpm = Get-Tpm
    Write-Output "tpm present:   $($tpm.TpmPresent), enabled: $($tpm.TpmEnabled)"
}

Section "Graphics"
# The PCI ID here is what plexos-gpu matches on, and what decides whether this
# generation uses the iHD or the legacy i965 VA-API driver.
Try-Report "video controllers" {
    Get-CimInstance Win32_VideoController | ForEach-Object {
        Write-Output "--- $($_.Name)"
        Write-Output "    pci id:  $(Get-PciId $_.PNPDeviceID)"
        Write-Output "    driver:  $($_.DriverVersion)"
        Write-Output "    raw id:  $($_.PNPDeviceID)"
    }
}

Section "Network controllers"
# Needed for the kernel config: a kernel without the right NIC driver built in gives a
# machine that boots and cannot be reached.
Try-Report "network adapters" {
    Get-CimInstance Win32_NetworkAdapter -Filter "PhysicalAdapter=True" | ForEach-Object {
        Write-Output "--- $($_.Name)"
        Write-Output "    pci id:  $(Get-PciId $_.PNPDeviceID)"
        Write-Output "    raw id:  $($_.PNPDeviceID)"
    }
}

Section "Storage controllers"
# Same reasoning: NVMe and AHCI drivers must be built into the kernel.
Try-Report "storage controllers" {
    Get-PnpDevice -PresentOnly -Class 'SCSIAdapter', 'HDC' -ErrorAction Stop |
        ForEach-Object {
            Write-Output "--- $($_.FriendlyName)"
            Write-Output "    pci id:  $(Get-PciId $_.InstanceId)"
        }
}

Section "Disks"
# MediaLith installs to a whole disk (ADR-0003). This is what would be overwritten.
Try-Report "disks" {
    Get-Disk | ForEach-Object {
        Write-Output "--- disk $($_.Number): $($_.FriendlyName)"
        Write-Output "    size:      $([math]::Round($_.Size / 1GB, 1)) GB"
        Write-Output "    bus:       $($_.BusType)"
        Write-Output "    partition: $($_.PartitionStyle)"
        Write-Output "    boot disk: $($_.IsBoot)"
    }
}

Section "Audio / other Intel devices"
# Occasionally useful when an iGPU is hidden or a chipset is unusual.
Try-Report "system devices" {
    Get-PnpDevice -PresentOnly -Class 'MEDIA', 'System' -ErrorAction Stop |
        Where-Object { $_.InstanceId -match 'VEN_8086' } |
        Select-Object -First 20 |
        ForEach-Object {
            Write-Output "$(Get-PciId $_.InstanceId)  $($_.FriendlyName)"
        }
}

Write-Output ""
Write-Output "===== end of capture ====="
Write-Output ""
Write-Output "Still needed from Linux (a live USB is enough):"
Write-Output "  - vainfo output          (VA-API capabilities)"
Write-Output "  - GuC/HuC status         (transcode quality on Intel)"
Write-Output "Run tools/capture-hardware.sh there for those."
