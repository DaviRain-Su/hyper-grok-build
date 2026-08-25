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

    # Strict whole-manifest parse: every non-empty line is
    #   <64 hex><space>[*]<basename>
    # Basename is a single path segment. Duplicate/case-colliding names fail.
    # Exactly one entry must name $Asset.
    $SeenNames = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    $Expected = $null
    $ExpectedCount = 0
    $LineNo = 0
    foreach ($rawLine in Get-Content -LiteralPath $SumsPath) {
        $LineNo++
        if ($null -eq $rawLine) { continue }
        $line = $rawLine.Trim()
        if ($line.Length -eq 0) { continue }
        foreach ($ch in $line.ToCharArray()) {
            if ([int][char]$ch -lt 32) {
                Fail "SHA256SUMS line $LineNo contains control characters"
            }
        }
        $parts = $line -split '\s+', 3
        if ($parts.Count -lt 2) {
            Fail "SHA256SUMS line $LineNo is malformed"
        }
        if ($parts.Count -gt 2) {
            Fail "SHA256SUMS line $LineNo has trailing fields"
        }
        $hash = [string]$parts[0]
        $name = ([string]$parts[1]).TrimStart('*')
        if ($hash -notmatch '^[0-9A-Fa-f]{64}$') {
            Fail "SHA256SUMS line $LineNo has an invalid digest"
        }
        if ($name -notmatch '^[A-Za-z0-9._+-]+$') {
            Fail "SHA256SUMS line $LineNo has an illegal asset name: $name"
        }
        if ($name.Contains('..')) {
            Fail "SHA256SUMS line $LineNo has an illegal asset name: $name"
        }
        if (-not $SeenNames.Add($name)) {
            Fail "SHA256SUMS contains duplicate or case-colliding entry for $name"
        }
        if ($name -eq $Asset) {
            $Expected = $hash.ToLowerInvariant()
            $ExpectedCount++
        }
    }
    if ($ExpectedCount -ne 1 -or $null -eq $Expected) {
        Fail "SHA256SUMS must contain exactly one entry for $Asset"
    }

    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "SHA256 mismatch for ${Asset}: expected $Expected, got $Actual"
    }
    Write-Host "Checksum verified."

    # ── Extract + install ────────────────────────────────────────────────────
    # Strict zip pre-scan: reject traversal, absolute/drive paths, reparse /
    # non-regular types, duplicate/case collisions, size/entry budgets, and
    # unexpected root entries. CI also gates archives with the Rust verifier.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $BinaryPath = Join-Path $TmpDir "hyper.exe"
    $BundledSource = Join-Path $TmpDir "bundled"
    $GrokHome = if ($env:GROK_HOME) { $env:GROK_HOME } else { Join-Path $HOME ".grok" }
    $BundledDest = Join-Path $GrokHome "bundled"
    $BundledStage = Join-Path $GrokHome ("bundled.install." + [System.IO.Path]::GetRandomFileName())
    $BundledAside = Join-Path $GrokHome ("bundled.old." + [System.IO.Path]::GetRandomFileName())

    $MaxEntries = 4096
    $MaxBinaryBytes = 1GB
    $MaxBundleFileBytes = 32MB
    $MaxBundleTotalBytes = 512MB
    $MaxBundleFiles = 4096
    $AllowedNotices = @("LICENSE", "NOTICE", "THIRD-PARTY-NOTICES", "THIRD-PARTY-NOTICES.md")

    function Test-SafeZipName([string]$Raw) {
        if ([string]::IsNullOrEmpty($Raw)) { return $false }
        if ($Raw.IndexOf([char]0) -ge 0) { return $false }
        foreach ($ch in $Raw.ToCharArray()) {
            if ([int][char]$ch -lt 32) { return $false }
        }
        $n = $Raw.Replace('\', '/')
        if ($n.StartsWith('/') -or $n.StartsWith('\\')) { return $false }
        if ($n.Length -ge 2 -and $n[1] -eq ':') { return $false }
        $parts = $n.TrimStart('./').Split('/') | Where-Object { $_ -ne '' -and $_ -ne '.' }
        foreach ($p in $parts) {
            if ($p -eq '..') { return $false }
            if ($p.Contains(':') -or $p.EndsWith('.') -or $p.EndsWith(' ')) { return $false }
            $upper = $p.ToUpperInvariant()
            $base = ($upper -split '\.')[0]
            if (@('CON','PRN','AUX','NUL','COM1','COM2','COM3','COM4','COM5','COM6','COM7','COM8','COM9','LPT1','LPT2','LPT3','LPT4','LPT5','LPT6','LPT7','LPT8','LPT9') -contains $base) {
                return $false
            }
        }
        return $true
    }

    function Get-NormalizedZipName([string]$Raw) {
        $n = $Raw.Replace('\', '/').TrimStart('./')
        while ($n.StartsWith('./')) { $n = $n.Substring(2) }
        return $n.TrimEnd('/')
    }

    $Zip = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    $BinaryEntry = $null
    $BundledEntries = New-Object System.Collections.Generic.List[object]
    try {
        $entries = @($Zip.Entries)
        if ($entries.Count -gt $MaxEntries) {
            Fail "archive $Asset contains too many entries"
        }
        $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        $bundleFileCount = 0
        $bundleTotal = [int64]0
        $binaryCount = 0

        foreach ($entry in $entries) {
            $raw = $entry.FullName
            if (-not (Test-SafeZipName $raw)) {
                Fail "archive $Asset contains an unsafe path: $raw"
            }
            $isDir = $raw.EndsWith('/') -or $raw.EndsWith('\') -or $entry.FullName.EndsWith('/')
            $norm = Get-NormalizedZipName $raw
            if ([string]::IsNullOrEmpty($norm)) {
                if (-not $isDir) { Fail "archive $Asset contains an unnamed regular entry" }
                continue
            }
            if (-not $seen.Add($norm)) {
                Fail "archive $Asset contains duplicate or case-colliding entry: $norm"
            }

            # Reparse / symlink attributes (Windows high word or Unix mode).
            $attrs = [uint32]$entry.ExternalAttributes
            $unixType = ($attrs -shr 16) -band 0xF000
            # S_IFLNK = 0xA000; S_IFREG = 0x8000; S_IFDIR = 0x4000
            if ($unixType -eq 0xA000) {
                Fail "archive $Asset contains a symlink: $raw"
            }
            if ($unixType -ne 0 -and $unixType -ne 0x8000 -and $unixType -ne 0x4000) {
                Fail "archive $Asset contains a non-regular entry type: $raw"
            }
            # FILE_ATTRIBUTE_REPARSE_POINT = 0x400
            if (($attrs -band 0x400) -ne 0 -and $unixType -eq 0) {
                Fail "archive $Asset contains a reparse-point entry: $raw"
            }

            if ($norm -eq "hyper.exe") {
                if ($isDir) { Fail "archive $Asset hyper.exe must be a regular file" }
                if ($entry.Length -le 0 -or $entry.Length -gt $MaxBinaryBytes) {
                    Fail "archive $Asset contains an invalid-size hyper.exe"
                }
                if ($unixType -ne 0 -and $unixType -ne 0x8000) {
                    Fail "archive $Asset contains a non-regular hyper.exe entry"
                }
                $binaryCount++
                $BinaryEntry = $entry
                continue
            }
            if ($AllowedNotices -contains $norm) {
                if ($isDir) { Fail "archive $Asset notice entry must be a regular file: $norm" }
                continue
            }
            if ($norm -eq "bundled" -or $norm.StartsWith("bundled/")) {
                if ($norm -eq "bundled" -and -not $isDir -and $entry.Length -eq 0) {
                    # Some zip producers emit zero-length dir markers without trailing /
                    $isDir = $true
                }
                if (-not $isDir) {
                    if ($entry.Length -gt $MaxBundleFileBytes) {
                        Fail "archive $Asset bundle file exceeds per-file limit: $norm"
                    }
                    $bundleFileCount++
                    if ($bundleFileCount -gt $MaxBundleFiles) {
                        Fail "archive $Asset bundle has too many files"
                    }
                    $bundleTotal += $entry.Length
                    if ($bundleTotal -gt $MaxBundleTotalBytes) {
                        Fail "archive $Asset bundle exceeds total size limit"
                    }
                }
                $BundledEntries.Add($entry) | Out-Null
                continue
            }
            Fail "archive $Asset contains unexpected entry: $norm"
        }

        if ($binaryCount -ne 1 -or $null -eq $BinaryEntry) {
            Fail "archive $Asset must contain exactly one root-level hyper.exe"
        }

        [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
            $BinaryEntry, $BinaryPath, $true
        )
        $binItem = Get-Item -LiteralPath $BinaryPath -Force
        if (($binItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "extracted hyper.exe is a reparse point"
        }

        foreach ($entry in $BundledEntries) {
            $name = (Get-NormalizedZipName $entry.FullName)
            $isDir = $entry.FullName.EndsWith('/') -or $entry.FullName.EndsWith('\') -or $name -eq "bundled"
            if ($name -eq "bundled" -and -not $entry.FullName.EndsWith('/') -and $entry.Length -eq 0) {
                $isDir = $true
            }
            if ($name -eq "bundled") {
                $destPath = $BundledSource
            } else {
                $rel = $name.Substring("bundled/".Length)
                $destPath = Join-Path $BundledSource ($rel -replace '/', [IO.Path]::DirectorySeparatorChar)
            }
            if ($isDir -or $entry.FullName.EndsWith('/') -or $entry.FullName.EndsWith('\')) {
                New-Item -ItemType Directory -Path $destPath -Force | Out-Null
                continue
            }
            $parent = Split-Path -Parent $destPath
            if ($parent) {
                New-Item -ItemType Directory -Path $parent -Force | Out-Null
            }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
                $entry, $destPath, $true
            )
            $extracted = Get-Item -LiteralPath $destPath -Force
            if (($extracted.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                Fail "extracted bundle path is a reparse point: $destPath"
            }
        }
    } finally {
        $Zip.Dispose()
    }
    $Binary = Get-Item -LiteralPath $BinaryPath -Force

    # ── Layout preflight (all shape checks before any live rename) ───────────
    Ensure-SafeDirectory $HyperHome "Hyper install root"
    $BinDir = Join-Path $HyperHome "bin"
    Ensure-SafeDirectory $BinDir "Hyper bin directory"
    $Dest = Join-Path $BinDir "hyper.exe"
    $StatePath = Join-Path $HyperHome "update-state.json"

    # Capture prior deployment *before* any live-path rename.
    $Aside = "$Dest.old.$PID.$([Guid]::NewGuid().ToString('N'))"
    $StateAside = "$StatePath.old.$PID.$([Guid]::NewGuid().ToString('N'))"
    $HadPrior = $false
    $HadState = $false
    $HadBundle = $false
    $PrevStateBytes = $null

    if (Test-Path -LiteralPath $Dest) {
        $destItem = Get-Item -LiteralPath $Dest -Force
        if (($destItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point hyper executable: $Dest"
        }
        if ($destItem.PSIsContainer) {
            Fail "hyper install path is a directory: $Dest"
        }
        $HadPrior = $true
    }
    if (Test-Path -LiteralPath $StatePath) {
        $StateItem = Get-Item -LiteralPath $StatePath -Force
        if (($StateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point update state: $StatePath"
        }
        if ($StateItem.PSIsContainer) {
            Fail "Hyper update state is not a regular file: $StatePath"
        }
        $HadState = $true
        $PrevStateBytes = [IO.File]::ReadAllBytes($StatePath)
    }
    if (Test-Path -LiteralPath $BundledDest) {
        $bundleItem = Get-Item -LiteralPath $BundledDest -Force
        if (($bundleItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point bundled runtime: $BundledDest"
        }
        if (-not $bundleItem.PSIsContainer) {
            Fail "bundled runtime path is not a directory: $BundledDest"
        }
        $HadBundle = $true
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
    # Binary-only packages leave $HasBundle false so the old tree is kept.
    $HasBundle = $false
    if (Test-Path -LiteralPath $BundledSource) {
        Ensure-SafeDirectory $GrokHome "Grok home"
        if (Test-Path -LiteralPath $BundledStage) {
            Fail "bundle stage path already exists: $BundledStage"
        }
        Copy-Item -Path $BundledSource -Destination $BundledStage -Recurse -Force
        $HasBundle = $true
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

    # Compensating transaction state. All live renames run inside one try;
    # any throw (including test-injected throw at INJECT_FAIL_AFTER_STATE)
    # is caught, rolled back via Invoke-RollbackAll, then Fail'd.
    $script:ActivatedBinary = $false
    $script:ActivatedState = $false
    $script:ActivatedBundle = $false
    $script:MovedBinaryAside = $false
    $script:MovedStateAside = $false
    $script:MovedBundleAside = $false
    $script:RollbackErrors = New-Object System.Collections.Generic.List[string]
    $nl = [Environment]::NewLine
    # INJECT_AFTER_STATE_MARKER

    function Add-RollbackError([string]$Message) {
        $script:RollbackErrors.Add($Message) | Out-Null
    }

    function Invoke-RollbackBinary {
        if (-not $script:ActivatedBinary -and -not $script:MovedBinaryAside) { return }
        try {
            if (Test-Path -LiteralPath $Dest) {
                Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
            }
            if ($script:MovedBinaryAside -and (Test-Path -LiteralPath $Aside)) {
                Move-Item -LiteralPath $Aside -Destination $Dest -Force -ErrorAction Stop
                $script:MovedBinaryAside = $false
            }
        } catch {
            Add-RollbackError "binary: failed to restore previous executable from ${Aside}: $($_.Exception.Message)"
        }
        $script:ActivatedBinary = $false
    }

    function Invoke-RollbackState {
        if (-not $script:ActivatedState -and -not $script:MovedStateAside) { return }
        try {
            if (Test-Path -LiteralPath $StatePath) {
                Remove-Item -LiteralPath $StatePath -Force -ErrorAction SilentlyContinue
            }
            if ($script:MovedStateAside -and (Test-Path -LiteralPath $StateAside)) {
                Move-Item -LiteralPath $StateAside -Destination $StatePath -Force -ErrorAction Stop
                $script:MovedStateAside = $false
            } elseif ($HadState -and $null -ne $PrevStateBytes) {
                [IO.File]::WriteAllBytes($StatePath, $PrevStateBytes)
            }
        } catch {
            Add-RollbackError "state: failed to restore previous update-state: $($_.Exception.Message)"
        }
        $script:ActivatedState = $false
    }

    function Invoke-RollbackBundle {
        if (-not $script:ActivatedBundle -and -not $script:MovedBundleAside) {
            if ($HasBundle -and (Test-Path -LiteralPath $BundledStage)) {
                Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
            }
            return
        }
        try {
            if (Test-Path -LiteralPath $BundledDest) {
                $doomed = Join-Path $GrokHome ("bundled.failed." + [System.IO.Path]::GetRandomFileName())
                Move-Item -LiteralPath $BundledDest -Destination $doomed -Force -ErrorAction Stop
                if ($script:MovedBundleAside -and (Test-Path -LiteralPath $BundledAside)) {
                    Move-Item -LiteralPath $BundledAside -Destination $BundledDest -Force -ErrorAction Stop
                    $script:MovedBundleAside = $false
                    Remove-Item -LiteralPath $doomed -Recurse -Force -ErrorAction SilentlyContinue
                } else {
                    Remove-Item -LiteralPath $doomed -Recurse -Force -ErrorAction SilentlyContinue
                }
            } elseif ($script:MovedBundleAside -and (Test-Path -LiteralPath $BundledAside)) {
                Move-Item -LiteralPath $BundledAside -Destination $BundledDest -Force -ErrorAction Stop
                $script:MovedBundleAside = $false
            }
        } catch {
            Add-RollbackError "bundle: failed to restore previous tree from ${BundledAside}: $($_.Exception.Message)"
        }
        if ($HasBundle -and (Test-Path -LiteralPath $BundledStage)) {
            Remove-Item -LiteralPath $BundledStage -Recurse -Force -ErrorAction SilentlyContinue
        }
        $script:ActivatedBundle = $false
    }

    function Invoke-RollbackAll {
        # Reverse commit order: bundle → state → binary.
        Invoke-RollbackBundle
        Invoke-RollbackState
        Invoke-RollbackBinary
        if ($StateTmp -and (Test-Path -LiteralPath $StateTmp)) {
            Remove-Item -LiteralPath $StateTmp -Force -ErrorAction SilentlyContinue
            $script:StateTmp = $null
        }
    }

    try {
        # --- Activate binary ---
        # A running hyper.exe blocks writes but may allow renames.
        if ($HadPrior) {
            Move-Item -LiteralPath $Dest -Destination $Aside -ErrorAction Stop
            $script:MovedBinaryAside = $true
        }
        Move-Item -LiteralPath $Binary.FullName -Destination $Dest -ErrorAction Stop
        $script:ActivatedBinary = $true

        # Secondary smoke-test of the activated path.
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
            throw "installed binary failed to run ($ActiveSmokeDetail); previous install restored if available"
        }

        # --- Activate update-state.json ---
        if ($HadState) {
            Move-Item -LiteralPath $StatePath -Destination $StateAside -ErrorAction Stop
            $script:MovedStateAside = $true
        }
        Move-Item -LiteralPath $StateTmp -Destination $StatePath -ErrorAction Stop
        $StateTmp = $null
        $script:ActivatedState = $true
        # INJECT_FAIL_AFTER_STATE

        # --- Activate bundle (optional; binary-only keeps the old tree) ---
        if ($HasBundle -and (Test-Path -LiteralPath $BundledStage)) {
            if ($HadBundle) {
                Move-Item -LiteralPath $BundledDest -Destination $BundledAside -Force -ErrorAction Stop
                $script:MovedBundleAside = $true
            }
            Move-Item -LiteralPath $BundledStage -Destination $BundledDest -Force -ErrorAction Stop
            $script:ActivatedBundle = $true
        }
    } catch {
        $commitErr = $_.Exception.Message
        Invoke-RollbackAll
        if ($script:RollbackErrors.Count -gt 0) {
            $detail = ($script:RollbackErrors | ForEach-Object { "rollback error: $_" }) -join $nl
            Fail ("install failed and rollback was incomplete; installation may be inconsistent." + $nl + $nl + "commit error: $commitErr" + $nl + $detail)
        }
        Fail $commitErr
    }

    # Commit succeeded — best-effort cleanup of asides.
    if ($script:MovedBinaryAside -and (Test-Path -LiteralPath $Aside)) {
        # A still-running old image may keep this file locked. It is harmless
        # and a later install can remove it after that process exits.
        Remove-Item -LiteralPath $Aside -Force -ErrorAction SilentlyContinue
    }
    if ($script:MovedStateAside -and (Test-Path -LiteralPath $StateAside)) {
        Remove-Item -LiteralPath $StateAside -Force -ErrorAction SilentlyContinue
    }
    if ($script:MovedBundleAside) {
        Remove-Item -LiteralPath $BundledAside -Recurse -Force -ErrorAction SilentlyContinue
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
    if ($StateTmp -and (Test-Path -LiteralPath $StateTmp)) {
        Remove-Item -LiteralPath $StateTmp -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
