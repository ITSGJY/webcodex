# Windows-native release artifact packaging for WebCodex.
#
# Produces `webcodex-v<VERSION>-win32-x64.tar.gz` from three Windows release
# binaries. This is the Windows release path: it must run on a Windows host
# with nothing but PowerShell and the built-in Windows tooling. It never
# requires Git Bash, WSL, or Unix chmod/install/sha256sum.
#
# The archive keeps the current three-binary npm contract:
#
#   webcodex.exe webcodex-server.exe webcodex-runner.exe
#
# webcodex-server.exe is packaged to keep the artifact/manifest contract
# intact; it is NOT a statement that a long-running Windows Server runtime is
# supported (it is not, in this release).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\package_release_artifact.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\package_release_artifact.ps1 `
#       -BinDir E:\webcodex\target\release -OutDir E:\webcodex\dist
#
# Parameter defaults honor the same environment variables as the Unix
# packaging script: WEBCODEX_RELEASE_BIN_DIR and WEBCODEX_RELEASE_OUT_DIR.
#
# Release builds must produce one shared build identity across the three
# binaries. webcodex-core's build.rs honors WEBCODEX_BUILT_AT when set, so a
# release build should pin it once (e.g. `$env:WEBCODEX_BUILT_AT = ...` before
# `cargo build --release`) to keep `built_at` identical across the packages;
# see scripts/npm_install_windows_smoke.ps1 for the pattern.
#
# On success prints the SHA-256 and the archive path. On failure exits
# non-zero and never leaves a partial file under the final archive name.
[CmdletBinding()]
param(
    [string]$BinDir,
    [string]$OutDir,
    [string]$Version
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
if (-not $BinDir) {
    $BinDir = if ($env:WEBCODEX_RELEASE_BIN_DIR) {
        $env:WEBCODEX_RELEASE_BIN_DIR
    } else {
        Join-Path $Root "target\release"
    }
}
if (-not $OutDir) {
    $OutDir = if ($env:WEBCODEX_RELEASE_OUT_DIR) {
        $env:WEBCODEX_RELEASE_OUT_DIR
    } else {
        Join-Path $Root "dist"
    }
}

if (-not $Version) {
    $packageJson = Join-Path $Root "npm\webcodex\package.json"
    $Version = (Get-Content -LiteralPath $packageJson -Raw | ConvertFrom-Json).version
    if (-not $Version) {
        throw "cannot read package version from $packageJson"
    }
}
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+') {
    throw "invalid package version '$Version'"
}

$BinDir = [System.IO.Path]::GetFullPath($BinDir)
$OutDir = [System.IO.Path]::GetFullPath($OutDir)

$BinaryNames = @("webcodex", "webcodex-server", "webcodex-runner")
$ArchiveName = "webcodex-v$Version-win32-x64.tar.gz"
$Archive = Join-Path $OutDir $ArchiveName
$ArchiveTmp = "$Archive.tmp"

# Windows 10 1803+ / Windows 11 ship tar.exe (libarchive) at
# %SystemRoot%\System32\tar.exe; it is the supported archiver. It is used
# explicitly because Git Bash/MSYS puts its own tar (which mangles Windows
# paths and is not supported) earlier on PATH.
$Tar = Join-Path $env:SystemRoot "System32\tar.exe"
if (-not (Test-Path -LiteralPath $Tar -PathType Leaf)) {
    throw "tar.exe was not found at $Tar. Windows 11 ships tar.exe in System32; restore it (Windows Features). Git Bash tar is not supported for Windows release artifacts."
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Staging = Join-Path $OutDir (".win32-artifact-staging-" + [guid]::NewGuid().ToString("N"))
try {
    New-Item -ItemType Directory -Force -Path (Join-Path $Staging "package") | Out-Null

    # 1. All three release binaries must exist.
    foreach ($name in $BinaryNames) {
        $source = Join-Path $BinDir "$name.exe"
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "missing release binary: $source"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $Staging "package\$name.exe")
    }

    # 2. Every binary must report the package version.
    # 3. Every binary must report the same build revision identity.
    $identities = @()
    foreach ($name in $BinaryNames) {
        $binary = Join-Path $Staging "package\$name.exe"
        $output = & $binary --version
        if ($LASTEXITCODE -ne 0) {
            throw "$name.exe --version failed with exit code $LASTEXITCODE"
        }
        $line = @($output | Select-Object -First 1)
        if (-not $line -or -not $line[0]) {
            throw "$name.exe --version produced no output"
        }
        $line = $line[0].TrimEnd()
        $expectedPrefix = "$name $Version"
        if ($line -ne $expectedPrefix -and -not $line.StartsWith("$expectedPrefix ")) {
            throw "unexpected $name version output: '$line' (expected '$expectedPrefix ...')"
        }
        $identities += $line.Substring($name.Length).TrimStart()
    }
    if (@($identities | Select-Object -Unique).Count -ne 1) {
        throw "release binaries do not share one build identity: $($identities -join ' | ')"
    }

    # 4. Archive only the three binaries, at the archive root.
    Push-Location (Join-Path $Staging "package")
    try {
        & $Tar -czf $ArchiveTmp @($BinaryNames | ForEach-Object { "$_.exe" })
    } finally {
        Pop-Location
    }
    if ($LASTEXITCODE -ne 0) {
        throw "tar.exe failed to create $ArchiveName (exit code $LASTEXITCODE)"
    }

    # 5. Publish the SHA-256, then move the complete archive into place. The
    # final name is only ever created from a fully verified archive.
    $Hash = Get-FileHash -LiteralPath $ArchiveTmp -Algorithm SHA256
    Move-Item -LiteralPath $ArchiveTmp -Destination $Archive
    Write-Output "$($Hash.Hash.ToLower())  $ArchiveName"
    Write-Output $Archive
} finally {
    if (Test-Path -LiteralPath $ArchiveTmp) {
        Remove-Item -LiteralPath $ArchiveTmp -Force
    }
    if (Test-Path -LiteralPath $Staging) {
        Remove-Item -LiteralPath $Staging -Recurse -Force
    }
}
