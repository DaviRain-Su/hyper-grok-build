#
# Grok CLI installer for PowerShell — https://x.ai/cli/install.ps1
#
# Auth: GROK_DEPLOYMENT_KEY env var (takes precedence) or ~/.grok/auth.json from `grok login`.
# Env: GROK_CHANNEL (stable|alpha|enterprise, default: stable), GROK_BIN_DIR, GROK_PROXY_URL
#
# Usage:
#   irm https://x.ai/cli/install.ps1 | iex                                       # latest stable
#   & ([scriptblock]::Create((irm https://x.ai/cli/install.ps1))) -Version 0.1.42 # specific version
#   $env:GROK_VERSION="0.1.42"; irm https://x.ai/cli/install.ps1 | iex           # specific version (alt)
#   $env:GROK_DEPLOYMENT_KEY="<key>"; irm https://x.ai/cli/install.ps1 | iex
#

param(
    [Parameter(Position = 0)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'

# PS 5.1 defaults to TLS 1.0; GCS requires TLS 1.2.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# PS 5.1's Invoke-WebRequest progress bar is extremely slow; disable it.
$ProgressPreference = 'SilentlyContinue'

# Accept version from environment variable (useful with irm | iex).
if (-not $Version -and $env:GROK_VERSION) {
    $Version = $env:GROK_VERSION
}

# This script is Windows-only. PS 5.1 has no Platform property and only runs on Windows.
if ($PSVersionTable.Platform -and $PSVersionTable.Platform -ne 'Win32NT') {
    Write-Error "This installer is for Windows. On macOS/Linux, use: curl -fsSL https://x.ai/cli/install.sh | bash"
    exit 1
}

$GrokDir = Join-Path $env:USERPROFILE '.grok'

# --- Helpers ---

function Download-String([string]$Url) {
    try {
        $response = Invoke-WebRequest -Uri $Url -UseBasicParsing
        return $response.Content
    } catch {
        return $null
    }
}

function Download-File([string]$Url, [string]$OutFile) {
    # TODO: parallel byte-range download (matches install.sh download_file_parallel).
    # Skipped for now: requires Start-ThreadJob / RunspacePool for true parallelism on PS 5.1
    # and HEAD + Range request orchestration. Single-connection HttpWebRequest below remains.
    # Stream via HttpWebRequest — faster than Invoke-WebRequest on PS 5.1 and supports progress.
    $request = [System.Net.HttpWebRequest]::Create($Url)
    $request.Timeout = 300000  # 5 min
    $request.AutomaticDecompression = [System.Net.DecompressionMethods]::GZip -bor [System.Net.DecompressionMethods]::Deflate
    $response = $request.GetResponse()
    $totalBytes = $response.ContentLength
    $stream = $response.GetResponseStream()
    $fileStream = [System.IO.File]::Create($OutFile)
    $buffer = New-Object byte[] 65536
    $totalRead = 0
    $lastPercent = -1
    $lastMb = -1

    try {
        while (($read = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $fileStream.Write($buffer, 0, $read)
            $totalRead += $read
            $mb = [math]::Round($totalRead / 1MB, 1)
            if ($totalBytes -gt 0) {
                $percent = [math]::Min(100, [math]::Floor(($totalRead / $totalBytes) * 100))
                if ($percent -ne $lastPercent) {
                    $totalMb = [math]::Round($totalBytes / 1MB, 1)
                    Write-Host "`r  Downloading... ${mb} MB / ${totalMb} MB (${percent}%)" -NoNewline
                    $lastPercent = $percent
                }
            } elseif ($mb -ne $lastMb) {
                Write-Host "`r  Downloading... ${mb} MB" -NoNewline
                $lastMb = $mb
            }
        }
        Write-Host ''
    } finally {
        $fileStream.Close()
        $stream.Close()
        $response.Close()
    }
}

function Read-GrokToken([string]$Scope) {
    $authFile = Join-Path $GrokDir 'auth.json'
    if (-not (Test-Path $authFile)) { return $null }
    try {
        $auth = Get-Content -Raw $authFile | ConvertFrom-Json
        $entry = $auth.$Scope
        if ($entry -and $entry.key) { return $entry.key }
    } catch {}
    return $null
}

# --- Validate version ---

if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(-\S+)?$') {
    Write-Error "Invalid version format: $Version (expected X.Y.Z or X.Y.Z-suffix)"
    exit 1
}

# --- Resolve auth ---

$OidcScope = 'https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828'
$LegacyScope = 'https://accounts.x.ai/sign-in'
$AuthSource = ''

if ($env:GROK_DEPLOYMENT_KEY) {
    $AuthSource = 'deployment key'
    Write-Host 'Auth: using deployment key.' -ForegroundColor DarkGray
} else {
    $oidcToken = Read-GrokToken $OidcScope
    $legacyToken = Read-GrokToken $LegacyScope
    if ($oidcToken) {
        $AuthSource = 'auth.json (oidc)'
        Write-Host 'Auth: using OIDC token from ~/.grok/auth.json.' -ForegroundColor DarkGray
    } elseif ($legacyToken) {
        $AuthSource = 'auth.json (legacy)'
        Write-Host 'Auth: using legacy token from ~/.grok/auth.json.' -ForegroundColor DarkGray
    }
}

# --- Detect architecture ---

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64'   { 'x86_64' }
    'x86'     { 'x86_64' }   # 32-bit PS on 64-bit Windows
    'ARM64'   { 'aarch64' }
    default   { $null }
}

if (-not $arch) {
    Write-Error "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE"
    exit 1
}

$platform = "windows-$arch"

# --- Resolve version and channel ---

$BaseUrlPrimary = 'https://x.ai/cli'
$BaseUrlFallback = 'https://storage.googleapis.com/grok-build-public-artifacts/cli'
$DownloadDir = Join-Path $GrokDir 'downloads'
$BinDir = if ($env:GROK_BIN_DIR) { $env:GROK_BIN_DIR } else { Join-Path $GrokDir 'bin' }

New-Item -ItemType Directory -Path $DownloadDir -Force | Out-Null
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null

$Channel = if ($env:GROK_CHANNEL) { $env:GROK_CHANNEL } else { 'stable' }

# Pick a working BaseUrl: try Cloudflare-fronted x.ai first, fall back to
# direct GCS if it's unreachable. The probe doubles as the channel-pointer
# fetch when no -Version was passed, so the happy path costs zero extra requests.
if (-not $Version) { Write-Host "Fetching latest $Channel version..." -ForegroundColor DarkGray }
$probeResult = Download-String "$BaseUrlPrimary/$Channel"
if ($probeResult) {
    $BaseUrl = $BaseUrlPrimary
} else {
    Write-Host "Note: $BaseUrlPrimary unreachable, falling back to direct GCS." -ForegroundColor Yellow
    $BaseUrl = $BaseUrlFallback
    $probeResult = Download-String "$BaseUrl/$Channel"
}

if ($Version) {
    $resolvedVersion = $Version
} elseif ($probeResult) {
    $resolvedVersion = $probeResult.Trim()
} else {
    Write-Error "Failed to fetch latest version from $BaseUrlPrimary/$Channel and $BaseUrlFallback/$Channel"
    exit 1
}

if ($AuthSource) {
    Write-Host "Installing Grok $resolvedVersion ($platform, $AuthSource)..." -ForegroundColor Cyan
} else {
    Write-Host "Installing Grok $resolvedVersion ($platform)..." -ForegroundColor Cyan
}

# --- Download binary ---

$binaryPath = Join-Path $DownloadDir "grok-$platform.exe"
$artifactBase = "$BaseUrl/grok-$resolvedVersion-$platform"

$downloaded = $false
foreach ($url in @("$artifactBase.exe", $artifactBase)) {
    try {
        Download-File $url $binaryPath
        $downloaded = $true
        break
    } catch {
        continue
    }
}

if (-not $downloaded) {
    if (Test-Path $binaryPath) { Remove-Item $binaryPath -Force }
    Write-Error "Binary download failed from $artifactBase.exe and $artifactBase"
    exit 1
}

# --- Install binary (locked-file safe) ---

foreach ($binName in @('grok.exe', 'agent.exe')) {
    $dest = Join-Path $BinDir $binName
    $old = "$dest.old"

    if (Test-Path $old) { Remove-Item $old -Force -ErrorAction SilentlyContinue }

    try {
        Copy-Item -Path $binaryPath -Destination $dest -Force
    } catch {
        try {
            if (Test-Path $dest) { Rename-Item $dest $old -Force -ErrorAction SilentlyContinue }
            Copy-Item -Path $binaryPath -Destination $dest -Force
        } catch {
            if (Test-Path $old) { Rename-Item $old $dest -Force -ErrorAction SilentlyContinue }
            Write-Error "Failed to install $binName"
            exit 1
        }
    }
}

Write-Host "  Installed to $BinDir\grok.exe and $BinDir\agent.exe." -ForegroundColor DarkGray

# --- Generate completions (best-effort) ---

$completionsDir = Join-Path (Join-Path $GrokDir 'completions') 'powershell'
try {
    New-Item -ItemType Directory -Path $completionsDir -Force | Out-Null
    & (Join-Path $BinDir 'grok.exe') completions powershell 2>$null |
        Set-Content (Join-Path $completionsDir 'grok.ps1') -ErrorAction SilentlyContinue
} catch {}

# --- Persist installer config ---

$ConfigFile = Join-Path $GrokDir 'config.toml'
$cliLines = @('installer = "internal"')
if ($Channel -ne 'stable') {
    $cliLines += "channel = `"$Channel`""
}

if (-not (Test-Path $ConfigFile)) {
    New-Item -ItemType Directory -Path (Split-Path $ConfigFile) -Force | Out-Null
    $content = "[cli]`r`n" + ($cliLines -join "`r`n") + "`r`n"
    [System.IO.File]::WriteAllText($ConfigFile, $content, [System.Text.Encoding]::UTF8)
} elseif ((Get-Content -Raw $ConfigFile) -match '(?m)^\[cli\]') {
    # Section-aware: only replace installer/channel under [cli], not other sections.
    $existingLines = Get-Content $ConfigFile
    $output = [System.Collections.ArrayList]::new()
    $inCli = $false

    foreach ($line in $existingLines) {
        if ($line -match '^\[cli\]\s*(#.*)?$') {
            [void]$output.Add($line)
            foreach ($cl in $cliLines) { [void]$output.Add($cl) }
            $inCli = $true
            continue
        }
        if ($line -match '^\[.+\]\s*(#.*)?$') {
            $inCli = $false
        }
        if ($inCli -and $line -match '^\s*(installer|channel)\s*=') {
            continue
        }
        [void]$output.Add($line)
    }
    [System.IO.File]::WriteAllLines($ConfigFile, [string[]]$output.ToArray(), [System.Text.Encoding]::UTF8)
} else {
    Add-Content -Path $ConfigFile -Value "`r`n[cli]`r`n$($cliLines -join "`r`n")`r`n"
}

# --- Fetch deployment config (deployment key only) ---

if ($env:GROK_DEPLOYMENT_KEY) {
    $ProxyUrl = if ($env:GROK_PROXY_URL) { $env:GROK_PROXY_URL } else { 'https://cli-chat-proxy.grok.com/v1' }
    Write-Host '  Fetching deployment config...' -ForegroundColor DarkGray
    try {
        $headers = @{ 'Authorization' = "Bearer $($env:GROK_DEPLOYMENT_KEY)" }
        $deployResponse = Invoke-RestMethod -Uri "$ProxyUrl/deployment/config" -Headers $headers -UseBasicParsing
    } catch {
        Write-Host "  Warning: failed to fetch deployment config from $ProxyUrl/deployment/config" -ForegroundColor Yellow
        $deployResponse = $null
    }

    if ($deployResponse) {
        $managedConfig = $deployResponse.managed_config
        $requirements = $deployResponse.requirements

        $managedConfigPath = Join-Path $GrokDir 'managed_config.toml'
        $requirementsPath = Join-Path $GrokDir 'requirements.toml'

        if ($managedConfig -and $managedConfig -ne 'null') {
            [System.IO.File]::WriteAllText($managedConfigPath, $managedConfig, [System.Text.Encoding]::UTF8)
            Write-Host '  Managed config applied.' -ForegroundColor DarkGray
        } else {
            if (Test-Path $managedConfigPath) { Remove-Item $managedConfigPath -Force }
        }

        if ($requirements -and $requirements -ne 'null') {
            [System.IO.File]::WriteAllText($requirementsPath, $requirements, [System.Text.Encoding]::UTF8)
            Write-Host '  Requirements applied.' -ForegroundColor DarkGray
        } else {
            if (Test-Path $requirementsPath) { Remove-Item $requirementsPath -Force }
        }
    }
}

Write-Host "Grok $resolvedVersion installed to $BinDir\grok.exe" -ForegroundColor Green

# --- Ensure grok is on PATH ---

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = if ($userPath) { $userPath -split ';' | Where-Object { $_ -ne '' } } else { @() }
if ($pathEntries -notcontains $BinDir) {
    $newPath = (@($BinDir) + $pathEntries) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "  Added $BinDir to your User PATH." -ForegroundColor DarkGray
    # Update current session so grok works immediately.
    if ($env:Path -notlike "*$BinDir*") {
        $env:Path = "$BinDir;$env:Path"
    }
}

Write-Host ''
Write-Host "Run 'grok' or 'agent' to get started!" -ForegroundColor Cyan
