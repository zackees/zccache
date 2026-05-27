[CmdletBinding()]
param(
    [switch]$Global,
    [switch]$User,
    [string]$BinDir,
    [string]$Version = $(if ($env:ZCCACHE_INSTALL_VERSION) { $env:ZCCACHE_INSTALL_VERSION } else { "latest" })
)

$ErrorActionPreference = "Stop"

function Write-Log {
    param([string]$Message)
    Write-Host "[zccache-install] $Message"
}

function Get-InstallMode {
    if ($Global.IsPresent) { return "global" }
    if ($User.IsPresent) { return "user" }
    if ($env:ZCCACHE_INSTALL_MODE) { return $env:ZCCACHE_INSTALL_MODE.ToLowerInvariant() }
    return "user"
}

function Resolve-VersionTag {
    param([string]$RawVersion)
    if ($RawVersion -eq "latest") { return Get-LatestReleaseTag }
    return $RawVersion
}

function Get-AssetTag {
    param([string]$Tag)
    if ($Tag.StartsWith("v")) { return $Tag }
    return "v$Tag"
}

function Get-ReleaseBaseUrl {
    if ($env:ZCCACHE_INSTALL_BASE_URL) {
        return $env:ZCCACHE_INSTALL_BASE_URL.TrimEnd("/")
    }
    $repo = if ($env:ZCCACHE_INSTALL_REPO) { $env:ZCCACHE_INSTALL_REPO } else { "zackees/zccache" }
    return "https://github.com/$repo/releases"
}

function Get-LatestReleaseTag {
    $latestUrl = "$(Get-ReleaseBaseUrl)/latest"
    $response = Invoke-WebRequest -Uri $latestUrl -MaximumRedirection 10
    $finalUri = $null
    if ($response.BaseResponse.ResponseUri) {
        $finalUri = $response.BaseResponse.ResponseUri
    } elseif ($response.BaseResponse.RequestMessage -and $response.BaseResponse.RequestMessage.RequestUri) {
        $finalUri = $response.BaseResponse.RequestMessage.RequestUri
    }
    if (-not $finalUri) {
        throw "Could not resolve latest release tag from $latestUrl"
    }
    $tag = $finalUri.AbsolutePath.TrimEnd("/").Split("/")[-1]
    if (-not $tag -or $tag -eq "latest") {
        throw "Could not resolve latest release tag from $latestUrl"
    }
    return $tag
}

function Get-TargetTriple {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        ([System.Runtime.InteropServices.Architecture]::X64) { $cpu = "x86_64" }
        ([System.Runtime.InteropServices.Architecture]::Arm64) { $cpu = "aarch64" }
        default { throw "Unsupported architecture: $arch" }
    }
    return "$cpu-pc-windows-msvc"
}

function Get-AssetUrl {
    param([string]$Tag, [string]$Asset)
    $baseUrl = Get-ReleaseBaseUrl
    return "$baseUrl/download/$Tag/$Asset"
}

function Add-UserPath {
    param([string]$InstallDir)
    if (($env:PATH -split ';') -contains $InstallDir) {
        return
    }
    if ($env:ZCCACHE_NO_MODIFY_PATH -eq "1") {
        return
    }
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($current) {
        $parts = $current -split ';' | Where-Object { $_ }
    }
    if ($parts -contains $InstallDir) {
        return
    }
    $newPath = if ($current) { "$current;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Log "Added $InstallDir to the user PATH."
}

$installMode = Get-InstallMode
$installDir = if ($BinDir) {
    $BinDir
} elseif ($env:ZCCACHE_INSTALL_DIR) {
    $env:ZCCACHE_INSTALL_DIR
} elseif ($installMode -eq "global") {
    Join-Path ${env:ProgramFiles} "zccache\bin"
} else {
    Join-Path $HOME ".local\bin"
}

$tag = Resolve-VersionTag $Version
$target = Get-TargetTriple
$archiveTag = Get-AssetTag $tag
$asset = "zccache-$archiveTag-$target.zip"
$url = Get-AssetUrl -Tag $tag -Asset $asset

$tmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("zccache-install-" + [guid]::NewGuid().ToString("N"))
$archivePath = Join-Path $tmpRoot $asset
$extractDir = Join-Path $tmpRoot "extract"

try {
    New-Item -ItemType Directory -Force -Path $tmpRoot | Out-Null
    Write-Log "Downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $archivePath
    Microsoft.PowerShell.Archive\Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force

    $archiveRoot = Join-Path $extractDir "zccache-$archiveTag-$target"
    if (-not (Test-Path -LiteralPath $archiveRoot)) {
        throw "Archive layout was not recognized."
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -LiteralPath (Join-Path $archiveRoot "zccache.exe") -Destination (Join-Path $installDir "zccache.exe") -Force
    Copy-Item -LiteralPath (Join-Path $archiveRoot "zccache-daemon.exe") -Destination (Join-Path $installDir "zccache-daemon.exe") -Force
    $fp = Join-Path $archiveRoot "zccache-fp.exe"
    if (Test-Path -LiteralPath $fp) {
        Copy-Item -LiteralPath $fp -Destination (Join-Path $installDir "zccache-fp.exe") -Force
    }

    if ($installMode -eq "user") {
        Add-UserPath -InstallDir $installDir
    }

    Write-Log "Installed to $installDir"
    if (-not (($env:PATH -split ';') -contains $installDir)) {
        Write-Log "Open a new shell or add $installDir to PATH before running zccache."
    }
} finally {
    if (Test-Path -LiteralPath $tmpRoot) {
        Remove-Item -LiteralPath $tmpRoot -Force -Recurse
    }
}
