# Hyper installer (Windows x86_64).
#
# Downloads the x86_64-pc-windows-msvc artifact from this repo's GitHub
# Releases, verifies its SHA-256 against the release's SHA256SUMS manifest,
# and installs the binary as %USERPROFILE%\.hyper\bin\hyper.exe.
#
# Usage:
#   irm https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.ps1 | iex
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.1.0
#
# Environment:
#   HYPER_SHARE_DIR        install root (default: %USERPROFILE%\.hyper)
#   HYPER_UPDATE_BASE_URL  GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/DaviRain-Su/hyper-grok-build/releases)

[CmdletBinding()]
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Fail([string]$Message) {
    Write-Error "install.ps1: error: $Message"
    exit 1
}

$Repo = "DaviRain-Su/hyper-grok-build"
$ApiBase = if ($env:HYPER_UPDATE_BASE_URL) { $env:HYPER_UPDATE_BASE_URL } else { "https://api.github.com/repos/$Repo/releases" }
$HyperHome = if ($env:HYPER_SHARE_DIR) { $env:HYPER_SHARE_DIR } else { Join-Path $env:USERPROFILE ".hyper" }
$Triple = "x86_64-pc-windows-msvc"

# ── Platform gate ────────────────────────────────────────────────────────────
if (-not [System.Environment]::Is64BitOperatingSystem) {
    Fail "hyper requires 64-bit Windows (x86_64)"
}
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne "AMD64") {
    Fail "unsupported architecture '$arch' (only x86_64/AMD64 Windows builds are published)"
}

# ── Version argument ─────────────────────────────────────────────────────────
$Version = $Version.TrimStart("v")
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$') {
    Fail "invalid version '$Version' (expected X.Y.Z or vX.Y.Z)"
}

# TLS 1.2 for older PowerShell 5.1 defaults.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Headers = @{ "User-Agent" = "hyper-install"; "Accept" = "application/vnd.github+json" }

# ── Resolve the release ──────────────────────────────────────────────────────
$ReleaseUrl = if ($Version) { "$ApiBase/tags/v$Version" } else { "$ApiBase/latest" }
Write-Host "Resolving release from $ReleaseUrl"
try {
    $Release = Invoke-RestMethod -Uri $ReleaseUrl -Headers $Headers
} catch {
    Fail "could not fetch release metadata from ${ReleaseUrl}: $($_.Exception.Message)"
}

$Tag = [string]$Release.tag_name
if (-not $Tag) { Fail "release metadata has no tag_name (endpoint: $ReleaseUrl)" }
$ResolvedVersion = $Tag.TrimStart("v")
if ($Version -and $ResolvedVersion -ne $Version) {
    Fail "requested version $Version but release tag is $Tag"
}

$Asset = "hyper-$ResolvedVersion-$Triple.zip"
$ArchiveAsset = $Release.assets | Where-Object { $_.name -eq $Asset } | Select-Object -First 1
$SumsAsset = $Release.assets | Where-Object { $_.name -eq "SHA256SUMS" } | Select-Object -First 1
if (-not $ArchiveAsset) { Fail "release $Tag has no asset $Asset" }
if (-not $SumsAsset) { Fail "release $Tag has no SHA256SUMS asset; refusing to install unverified binaries" }

# ── Download + verify ────────────────────────────────────────────────────────
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("hyper-install-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
try {
    $ArchivePath = Join-Path $TmpDir $Asset
    $SumsPath = Join-Path $TmpDir "SHA256SUMS"

    Write-Host "Downloading hyper v$ResolvedVersion ($Triple)..."
    Invoke-WebRequest -Uri $ArchiveAsset.browser_download_url -Headers $Headers -OutFile $ArchivePath
    Invoke-WebRequest -Uri $SumsAsset.browser_download_url -Headers $Headers -OutFile $SumsPath

    $Expected = $null
    foreach ($line in Get-Content $SumsPath) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Asset) {
            $Expected = $parts[0].ToLowerInvariant()
        }
    }
    if (-not $Expected) { Fail "SHA256SUMS has no entry for $Asset" }

    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "SHA256 mismatch for ${Asset}: expected $Expected, got $Actual"
    }
    Write-Host "Checksum verified."

    # ── Extract + install ────────────────────────────────────────────────────
    $ExtractDir = Join-Path $TmpDir "extracted"
    Expand-Archive -Path $ArchivePath -DestinationPath $ExtractDir -Force
    $Binary = Get-ChildItem -Path $ExtractDir -Recurse -Filter "hyper.exe" | Select-Object -First 1
    if (-not $Binary) { Fail "archive $Asset does not contain hyper.exe" }

    $BinDir = Join-Path $HyperHome "bin"
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    $Dest = Join-Path $BinDir "hyper.exe"

    # A running hyper.exe blocks writes but allows renames — move it aside first.
    if (Test-Path $Dest) {
        $Aside = "$Dest.old"
        Remove-Item -Path $Aside -Force -ErrorAction SilentlyContinue
        try {
            Move-Item -Path $Dest -Destination $Aside -Force
        } catch {
            Fail "cannot replace $Dest (close all running hyper sessions and retry): $($_.Exception.Message)"
        }
    }
    Move-Item -Path $Binary.FullName -Destination $Dest -Force

    # Smoke-test the installed binary.
    & $Dest --version *> $null
    if ($LASTEXITCODE -ne 0) {
        Fail "installed binary failed to run (exit $LASTEXITCODE)"
    }

    Write-Host ""
    Write-Host "hyper v$ResolvedVersion installed to $Dest"

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $OnPath = (($UserPath -split ";") -contains $BinDir) -or
              (($env:Path -split ";") -contains $BinDir)
    if (-not $OnPath) {
        $NewUserPath = if ($UserPath) { "$BinDir;$UserPath" } else { $BinDir }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
        Write-Host ""
        Write-Host "Added $BinDir to your user PATH."
        Write-Host "Open a new terminal, then run 'hyper' to get started."
    } else {
        Write-Host "Run 'hyper' to get started."
    }
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
