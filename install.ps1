# Infigraph Windows installer
# Usage: iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
#
# Override for GitHub Enterprise:
#   $env:INFIGRAPH_GH_HOST = "github.example.com"; $env:INFIGRAPH_GH_OWNER = "myorg"

$ErrorActionPreference = "Stop"

$GHE_HOST  = if ($env:INFIGRAPH_GH_HOST) { $env:INFIGRAPH_GH_HOST } else { "github.com" }
$GHE_OWNER = if ($env:INFIGRAPH_GH_OWNER) { $env:INFIGRAPH_GH_OWNER } else { "intuit" }
$GHE_REPO  = "infigraph"
$INSTALL_DIR = if ($env:INFIGRAPH_INSTALL_DIR) { $env:INFIGRAPH_INSTALL_DIR } else { "$env:USERPROFILE\.local\bin" }
$TARGET    = "x86_64-pc-windows-msvc"
$ASSET     = "infigraph-$TARGET.zip"

Write-Host "Infigraph installer"
Write-Host "==================="
Write-Host "Target:      $TARGET"
Write-Host "Install dir: $INSTALL_DIR"
Write-Host ""

function Reload-Path {
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH","Machine") + ";" + `
                [System.Environment]::GetEnvironmentVariable("PATH","User")
}

# For GHE: require gh CLI + auth. For public GitHub: use Invoke-WebRequest directly.
if ($GHE_HOST -ne "github.com") {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        Write-Host "Error: gh CLI not found. Required for GitHub Enterprise."
        Write-Host "Install it: winget install --id GitHub.cli -e"
        Write-Host "Then authenticate: gh auth login --hostname $GHE_HOST"
        exit 1
    }
    if ((gh auth status --hostname $GHE_HOST 2>&1) -match "not logged") {
        Write-Host "→ Authenticating with $GHE_HOST..."
        gh auth login --hostname $GHE_HOST
    }
}

function Move-RunningBinary {
    param([string]$ExePath)
    if (Test-Path $ExePath) {
        $oldPath = "$ExePath.old"
        Remove-Item $oldPath -Force -ErrorAction SilentlyContinue
        try {
            Rename-Item $ExePath $oldPath -Force -ErrorAction Stop
            Write-Host "  Renamed running $([System.IO.Path]::GetFileName($ExePath)) -> .old"
        } catch {
            Write-Host "  Warning: could not rename $([System.IO.Path]::GetFileName($ExePath)) (may not be running)"
        }
    }
}

function Cleanup-OldBinaries {
    foreach ($name in @("infigraph.exe.old", "infigraph-mcp.exe.old")) {
        $old = Join-Path $INSTALL_DIR $name
        Remove-Item $old -Force -ErrorAction SilentlyContinue
    }
}

function Install-Prebuilt {
    Write-Host "Checking for pre-built binary..."

    $downloadPath = "$env:TEMP\$ASSET"

    if ($GHE_HOST -eq "github.com") {
        # Public GitHub: direct download, no gh CLI needed
        $headers = @{}
        if ($env:GITHUB_TOKEN) {
            $headers["Authorization"] = "token $env:GITHUB_TOKEN"
        }
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$GHE_OWNER/$GHE_REPO/releases/latest" -Headers $headers -UseBasicParsing
            $releaseTag = $release.tag_name
        } catch {
            if ($_.Exception.Response.StatusCode -eq 403) {
                Write-Host "GitHub API rate limit exceeded (unauthenticated: 60 requests/hour)."
                Write-Host "Set GITHUB_TOKEN: `$env:GITHUB_TOKEN = 'ghp_xxx'; .\install.ps1"
            } else {
                Write-Host "No releases found."
            }
            return $false
        }
        Write-Host "Latest release: $releaseTag"

        $url = "https://github.com/$GHE_OWNER/$GHE_REPO/releases/download/$releaseTag/$ASSET"
        try {
            Invoke-WebRequest -Uri $url -OutFile $downloadPath -UseBasicParsing
        } catch {
            Write-Host "No binary for $TARGET in release $releaseTag."
            return $false
        }
    } else {
        # GHE: use gh CLI (requires auth)
        $releaseTag = gh api --hostname $GHE_HOST "repos/$GHE_OWNER/$GHE_REPO/releases/latest" --jq '.tag_name' 2>$null
        if (-not $releaseTag) {
            Write-Host "No releases found."
            return $false
        }
        Write-Host "Latest release: $releaseTag"

        gh release download $releaseTag `
            --repo "$GHE_HOST/$GHE_OWNER/$GHE_REPO" `
            --pattern $ASSET `
            --dir $env:TEMP `
            --clobber 2>$null
        if ($LASTEXITCODE -ne 0) {
            Write-Host "No binary for $TARGET in release $releaseTag."
            return $false
        }
    }

    New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null

    # Rename running binaries before overwriting (Windows locks running .exe files)
    Move-RunningBinary (Join-Path $INSTALL_DIR "infigraph.exe")
    Move-RunningBinary (Join-Path $INSTALL_DIR "infigraph-mcp.exe")

    Expand-Archive -Force -Path $downloadPath -DestinationPath $INSTALL_DIR
    Remove-Item $downloadPath -Force
    Cleanup-OldBinaries
    Write-Host "Installed pre-built binary to $INSTALL_DIR\"
    return $true
}

function Install-FromSource {
    Write-Host "Building from source..."

    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        Write-Host "Error: cmake not found. Install it first:"
        Write-Host "  winget install Kitware.CMake"
        exit 1
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "Error: cargo not found. Install Rust first:"
        Write-Host "  winget install Rustlang.Rustup"
        exit 1
    }
    if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) {
        Write-Host "Error: ninja not found. Install it first:"
        Write-Host "  winget install Ninja-build.Ninja"
        exit 1
    }

    $srcDir = "$env:TEMP\infigraph-build"
    if (Test-Path "$srcDir\.git") {
        Write-Host "Updating existing clone..."
        git -C $srcDir pull
    } else {
        Write-Host "Cloning from $GHE_HOST/$GHE_OWNER/$GHE_REPO..."
        Remove-Item $srcDir -Recurse -Force -ErrorAction SilentlyContinue
        if ($GHE_HOST -eq "github.com") {
            git clone "https://github.com/$GHE_OWNER/$GHE_REPO.git" $srcDir
        } else {
            gh repo clone "$GHE_OWNER/$GHE_REPO" $srcDir -- --hostname $GHE_HOST
        }
    }

    Write-Host "Building release (this may take several minutes)..."
    Push-Location $srcDir
    cargo build --release --target x86_64-pc-windows-msvc -p infigraph-cli -p infigraph-mcp
    Pop-Location

    New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null

    # Rename running binaries before overwriting (Windows locks running .exe files)
    Move-RunningBinary (Join-Path $INSTALL_DIR "infigraph.exe")
    Move-RunningBinary (Join-Path $INSTALL_DIR "infigraph-mcp.exe")

    Copy-Item "$srcDir\target\x86_64-pc-windows-msvc\release\infigraph.exe" $INSTALL_DIR
    Copy-Item "$srcDir\target\x86_64-pc-windows-msvc\release\infigraph-mcp.exe" $INSTALL_DIR
    Cleanup-OldBinaries
    Write-Host "Built and installed to $INSTALL_DIR\"
}

# Main flow
$prebuiltOk = Install-Prebuilt
if (-not $prebuiltOk) {
    Install-FromSource
}

# Add to user PATH if not already present
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -notlike "*$INSTALL_DIR*") {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$INSTALL_DIR", "User")
    $env:PATH = "$env:PATH;$INSTALL_DIR"
    Write-Host "Added $INSTALL_DIR to user PATH."
    Write-Host "Restart your shell for the PATH change to take effect in new terminals."
}

# Register MCP + primary search
Write-Host ""
$infigraphExe = "$INSTALL_DIR\infigraph.exe"
if (Test-Path $infigraphExe) {
    Write-Host "Registering as primary search for AI agents..."
    & $infigraphExe install
}

Write-Host ""
Write-Host "=============================="
Write-Host "Infigraph installed!"
Write-Host "=============================="
Write-Host ""
Write-Host "Next steps:"
Write-Host "  cd C:\your\project"
Write-Host "  infigraph index              # Index a project"
Write-Host "  infigraph rebuild            # Full reindex from scratch"
Write-Host "  infigraph search 'query'     # Search indexed code"
Write-Host ""
Write-Host "Manage installation:"
Write-Host "  infigraph update             # Refresh after rebuild"
Write-Host "  infigraph uninstall          # Remove all configs"
