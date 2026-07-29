# Hyper installer (Windows x86_64).
#
# Downloads the x86_64-pc-windows-msvc artifact from this repo's GitHub
# Releases, verifies its SHA-256 against the release's SHA256SUMS manifest,
# and installs the binary as %USERPROFILE%\.hyper\bin\hyper.exe.
#
# Usage:
#   irm https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.ps1 | iex
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.2.109
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

function Ensure-SafeDirectory([string]$Path, [string]$Label) {
    if (Test-Path -LiteralPath $Path) {
        $Item = Get-Item -LiteralPath $Path -Force
        if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to use reparse-point ${Label}: $Path"
        }
        if (-not $Item.PSIsContainer) {
            Fail "$Label is not a directory: $Path"
        }
    } else {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
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
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
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
if ($Tag -notmatch '^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    Fail "release tag '$Tag' is invalid (expected semantic version vX.Y.Z)"
}
$ResolvedVersion = $Tag.Substring(1)
if ($Version -and $ResolvedVersion -ne $Version) {
    Fail "requested version $Version but release tag is $Tag"
}

$Asset = "hyper-$ResolvedVersion-$Triple.zip"
if ($null -eq $Release.assets) { Fail "release $Tag has no assets" }
$ArchiveMatches = @($Release.assets | Where-Object { $_.name -eq $Asset })
$SumsMatches = @($Release.assets | Where-Object { $_.name -eq "SHA256SUMS" })
if ($ArchiveMatches.Count -ne 1) { Fail "release $Tag must contain exactly one asset named $Asset" }
if ($SumsMatches.Count -ne 1) { Fail "release $Tag must contain exactly one SHA256SUMS asset" }
$ArchiveAsset = $ArchiveMatches[0]
$SumsAsset = $SumsMatches[0]

# ── Download + verify ────────────────────────────────────────────────────────
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("hyper-install-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$StateTmp = $null
try {
    $ArchivePath = Join-Path $TmpDir $Asset
    $SumsPath = Join-Path $TmpDir "SHA256SUMS"

    Write-Host "Downloading hyper v$ResolvedVersion ($Triple)..."
    Invoke-WebRequest -Uri $ArchiveAsset.browser_download_url -Headers $Headers -OutFile $ArchivePath
    Invoke-WebRequest -Uri $SumsAsset.browser_download_url -Headers $Headers -OutFile $SumsPath

    if ((Get-Item -LiteralPath $SumsPath).Length -gt 1MB) {
        Fail "SHA256SUMS is unexpectedly large"
    }
    if ((Get-Item -LiteralPath $ArchivePath).Length -gt 1GB) {
        Fail "$Asset exceeds the 1 GiB safety limit"
    }

    $ExpectedMatches = @()
    foreach ($line in Get-Content -LiteralPath $SumsPath) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Asset) {
            $ExpectedMatches += [string]$parts[0]
        }
    }
    if ($ExpectedMatches.Count -ne 1) {
        Fail "SHA256SUMS must contain exactly one entry for $Asset"
    }
    $Expected = $ExpectedMatches[0].ToLowerInvariant()
    if ($Expected -notmatch '^[0-9a-f]{64}$') {
        Fail "SHA256SUMS contains an invalid digest for $Asset"
    }

    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "SHA256 mismatch for ${Asset}: expected $Expected, got $Actual"
    }
    Write-Host "Checksum verified."

    # ── Extract + install ────────────────────────────────────────────────────
    # Materialize only the unique root-level executable, plus optional
    # installer-owned `bundled/` assets. Nested path traversal is rejected.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $BinaryPath = Join-Path $TmpDir "hyper.exe"
    $BundledSource = Join-Path $TmpDir "bundled"
    $GrokHome = if ($env:GROK_HOME) { $env:GROK_HOME } else { Join-Path $HOME ".grok" }
    $BundledDest = Join-Path $GrokHome "bundled"
    $BundledStage = Join-Path $GrokHome ("bundled.install." + [System.IO.Path]::GetRandomFileName())
    $BundledAside = Join-Path $GrokHome ("bundled.old." + [System.IO.Path]::GetRandomFileName())
    $Zip = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        if (@($Zip.Entries).Count -gt 4096) {
            Fail "archive $Asset contains too many entries"
        }
        $BinaryEntries = @($Zip.Entries | Where-Object {
            $_.FullName -eq "hyper.exe" -or $_.FullName -eq "./hyper.exe"
        })
        if ($BinaryEntries.Count -ne 1) {
            Fail "archive $Asset must contain exactly one root-level hyper.exe"
        }
        $BinaryEntry = $BinaryEntries[0]
        if ($BinaryEntry.Length -le 0 -or $BinaryEntry.Length -gt 1GB) {
            Fail "archive $Asset contains an invalid-size hyper.exe"
        }
        $UnixFileType = (($BinaryEntry.ExternalAttributes -shr 16) -band 0xF000)
        if ($UnixFileType -ne 0 -and $UnixFileType -ne 0x8000) {
            Fail "archive $Asset contains a non-regular hyper.exe entry"
        }
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
            $BinaryEntry, $BinaryPath, $true
        )

        $BundledEntries = @($Zip.Entries | Where-Object {
            $name = $_.FullName.Replace('\', '/')
            ($name -eq "bundled" -or $name -eq "./bundled" -or
             $name.StartsWith("bundled/") -or $name.StartsWith("./bundled/")) -and
            (-not $name.Contains(".."))
        })
        foreach ($entry in $BundledEntries) {
            $name = $entry.FullName.Replace('\', '/').TrimStart('./')
            if ($name.Contains("..")) {
                Fail "archive $Asset contains an unsafe bundled path: $($entry.FullName)"
            }
            $destPath = Join-Path $TmpDir ($name -replace '/', [IO.Path]::DirectorySeparatorChar)
            if ($name.EndsWith('/')) {
                New-Item -ItemType Directory -Path $destPath -Force | Out-Null
                continue
            }
            $parent = Split-Path -Parent $destPath
            if ($parent) {
                New-Item -ItemType Directory -Path $parent -Force | Out-Null
            }
            if ($entry.Length -gt 0 -or -not $name.EndsWith('/')) {
                [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
                    $entry, $destPath, $true
                )
            }
        }
    } finally {
        $Zip.Dispose()
    }
    $Binary = Get-Item -LiteralPath $BinaryPath

    Ensure-SafeDirectory $HyperHome "Hyper install root"
    $BinDir = Join-Path $HyperHome "bin"
    Ensure-SafeDirectory $BinDir "Hyper bin directory"
    $Dest = Join-Path $BinDir "hyper.exe"
    $StatePath = Join-Path $HyperHome "update-state.json"
    if (Test-Path -LiteralPath $StatePath) {
        $StateItem = Get-Item -LiteralPath $StatePath -Force
        if (($StateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point update state: $StatePath"
        }
    }

    # Smoke-test the downloaded binary *before* replacing the active install.
    $PreSmokeError = $null
    $PreSmokeExit = $null
    try {
        & $Binary.FullName --version *> $null
        $PreSmokeExit = $LASTEXITCODE
    } catch {
        $PreSmokeError = $_.Exception.Message
    }
    if ($PreSmokeError -or $PreSmokeExit -ne 0) {
        $PreSmokeDetail = if ($PreSmokeError) { $PreSmokeError } else { "exit $PreSmokeExit" }
        Fail "downloaded binary failed smoke test ($PreSmokeDetail); existing install left untouched"
    }

    # Stage the installer-owned bundle only after the downloaded binary passes.
    # Whole-tree replacement removes stale managed files without touching skills/.
    if (Test-Path -LiteralPath $BundledSource) {
        New-Item -ItemType Directory -Path $GrokHome -Force | Out-Null
        Copy-Item -Path $BundledSource -Destination $BundledStage -Recurse -Force
    }

    # Prepare parseable updater state before touching the active executable.
    # Windows PowerShell 5.1's `-Encoding UTF8` writes a BOM, which serde_json
    # rejects, so write explicit UTF-8 without BOM.
    $StateTmp = "$StatePath.install.$PID.$([Guid]::NewGuid().ToString('N'))"
    $UnixEpoch = [DateTime]::new(1970, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
    $CheckedAtUnix = [long][Math]::Floor(([DateTime]::UtcNow - $UnixEpoch).TotalSeconds)
    $State = [ordered]@{
        installed_version = $ResolvedVersion
        installed_asset = $Asset
        installed_sha256 = $Expected
        installed_binary = "hyper.exe"
        checked_at_unix = $CheckedAtUnix
    }
    $Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    $StateJson = ($State | ConvertTo-Json) + [Environment]::NewLine
    [IO.File]::WriteAllText($StateTmp, $StateJson, $Utf8NoBom)

    # A running hyper.exe blocks writes but may allow renames. Use a unique
    # aside path so a locked backup from an older process cannot block updates.
    $Aside = "$Dest.old.$PID.$([Guid]::NewGuid().ToString('N'))"
    $HadPrior = Test-Path -LiteralPath $Dest
    if ($HadPrior) {
        try {
            Move-Item -LiteralPath $Dest -Destination $Aside
        } catch {
            Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
            Fail "cannot replace $Dest (close all running hyper sessions and retry): $($_.Exception.Message)"
        }
    }
    try {
        Move-Item -LiteralPath $Binary.FullName -Destination $Dest
    } catch {
        if ($HadPrior -and (Test-Path -LiteralPath $Aside)) {
            Move-Item -LiteralPath $Aside -Destination $Dest -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
        Fail "cannot install to $Dest: $($_.Exception.Message)"
    }

    # Secondary smoke-test of the activated path; restore prior binary on failure.
    $ActiveSmokeError = $null
    $ActiveSmokeExit = $null
    try {
        & $Dest --version *> $null
        $ActiveSmokeExit = $LASTEXITCODE
    } catch {
        $ActiveSmokeError = $_.Exception.Message
    }
    if ($ActiveSmokeError -or $ActiveSmokeExit -ne 0) {
        $ActiveSmokeDetail = if ($ActiveSmokeError) { $ActiveSmokeError } else { "exit $ActiveSmokeExit" }
        Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
        if ($HadPrior -and (Test-Path -LiteralPath $Aside)) {
            Move-Item -LiteralPath $Aside -Destination $Dest -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
        Fail "installed binary failed to run ($ActiveSmokeDetail); previous install restored if available"
    }

    # Commit updater state only after the activated path passes its smoke test.
    # If state activation fails, restore both prior state and executable.
    $StateAside = "$StatePath.old.$PID.$([Guid]::NewGuid().ToString('N'))"
    $HadState = Test-Path -LiteralPath $StatePath
    try {
        if ($HadState) {
            Move-Item -LiteralPath $StatePath -Destination $StateAside
        }
        Move-Item -LiteralPath $StateTmp -Destination $StatePath
        $StateTmp = $null
    } catch {
        Remove-Item -LiteralPath $StatePath -Force -ErrorAction SilentlyContinue
        if ($HadState -and (Test-Path -LiteralPath $StateAside)) {
            Move-Item -LiteralPath $StateAside -Destination $StatePath -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
        if ($HadPrior -and (Test-Path -LiteralPath $Aside)) {
            Move-Item -LiteralPath $Aside -Destination $Dest -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
        Fail "cannot record Hyper update state; previous install restored if available: $($_.Exception.Message)"
    }
    if ($HadState -and (Test-Path -LiteralPath $StateAside)) {
        Remove-Item -LiteralPath $StateAside -Force -ErrorAction SilentlyContinue
    }

    # Activate the complete bundle after the binary is known-good.
    if (Test-Path -LiteralPath $BundledStage) {
        try {
            if (Test-Path -LiteralPath $BundledDest) {
                Move-Item -LiteralPath $BundledDest -Destination $BundledAside -Force
            }
            Move-Item -LiteralPath $BundledStage -Destination $BundledDest -Force
        } catch {
            Remove-Item -LiteralPath $BundledDest -Recurse -Force -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $BundledAside) {
                Move-Item -LiteralPath $BundledAside -Destination $BundledDest -Force -ErrorAction SilentlyContinue
            }
            Fail "cannot activate bundled runtime; previous bundle restored if available: $($_.Exception.Message)"
        }
    }

    if ($HadPrior -and (Test-Path -LiteralPath $Aside)) {
        # A still-running old image may keep this file locked. It is harmless
        # and a later install can remove it after that process exits.
        Remove-Item -LiteralPath $Aside -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $BundledAside -Recurse -Force -ErrorAction SilentlyContinue

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
    if ($StateTmp -and (Test-Path -LiteralPath $StateTmp)) {
        Remove-Item -LiteralPath $StateTmp -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
