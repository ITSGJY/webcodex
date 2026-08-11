# Windows ARM64 compatibility smoke for the npm installer.
#
# Runs under native ARM64 Node.js, then installs the already-published Windows
# x64 WebCodex artifact through the normal npm postinstall path. This proves
# that a win32-arm64 host can select win32-x64 and execute the resulting x64
# binaries through Windows 11's compatibility layer. The pinned v0.3.4 release
# is a stable fixture so this smoke can run before the current version is
# published.
[CmdletBinding()]
param(
    [string]$FixtureVersion = "0.3.4"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot

if ($env:PROCESSOR_ARCHITECTURE -ne "ARM64") {
    throw "this smoke requires a Windows ARM64 host"
}
$NodePlatform = (& node -p "process.platform").Trim()
$NodeArch = (& node -p "process.arch").Trim()
if ($NodePlatform -ne "win32" -or $NodeArch -ne "arm64") {
    throw "this smoke requires native win32-arm64 Node.js, got $NodePlatform-$NodeArch"
}

$TempRoot = Join-Path $env:TEMP ("webcodex-arm64-compat-" + [guid]::NewGuid().ToString("N"))
$Archive = Join-Path $TempRoot "webcodex-win32-x64.tar.gz"
$Extract = Join-Path $TempRoot "x64-bin"
$Prefix = Join-Path $TempRoot "install-prefix"
$PackageDir = Join-Path $TempRoot "webcodex"
$ArtifactUrl = "https://github.com/yyjeqhc/webcodex/releases/download/v$FixtureVersion/webcodex-v$FixtureVersion-win32-x64.tar.gz"
$PreviousRegistry = $env:npm_config_registry

function Get-PeMachine([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64) {
        throw "PE file is too small: $Path"
    }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
    if ($peOffset -lt 0 -or $peOffset + 6 -gt $bytes.Length) {
        throw "PE header offset is invalid: $Path"
    }
    return [BitConverter]::ToUInt16($bytes, $peOffset + 4)
}

try {
    New-Item -ItemType Directory -Force -Path $TempRoot, $Extract | Out-Null

    Write-Host "downloading x64 compatibility fixture $ArtifactUrl"
    Invoke-WebRequest -Uri $ArtifactUrl -OutFile $Archive
    $Sha256 = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()

    $SystemTar = Join-Path $env:SystemRoot "System32\tar.exe"
    if (-not (Test-Path -LiteralPath $SystemTar -PathType Leaf)) {
        throw "tar.exe was not found at $SystemTar"
    }
    & $SystemTar -xzf $Archive -C $Extract
    if ($LASTEXITCODE -ne 0) {
        throw "failed to extract the x64 compatibility fixture"
    }

    foreach ($name in @("webcodex", "webcodex-server", "webcodex-runner")) {
        $binary = Join-Path $Extract "$name.exe"
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw "compatibility fixture is missing $name.exe"
        }
        $machine = Get-PeMachine $binary
        Write-Host "$name.exe PE machine=0x$($machine.ToString('X4'))"
        if ($machine -ne 0x8664) {
            throw "$name.exe is not an x64 PE binary"
        }
        $versionOutput = & $binary --version
        if ($LASTEXITCODE -ne 0 -or -not $versionOutput) {
            throw "$name.exe did not execute successfully under Windows ARM64 x64 emulation"
        }
    }

    # Copy the current installer implementation into an isolated package, but
    # give the temporary package the fixture's published version so the normal
    # manifest/binary identity checks remain authoritative rather than bypassed.
    Copy-Item -LiteralPath (Join-Path $Root "npm\webcodex") -Destination $TempRoot -Recurse
    $PackageJsonPath = Join-Path $PackageDir "package.json"
    $PackageJson = Get-Content -LiteralPath $PackageJsonPath -Raw | ConvertFrom-Json
    $PackageJson.version = $FixtureVersion
    [System.IO.File]::WriteAllText(
        $PackageJsonPath,
        (($PackageJson | ConvertTo-Json -Depth 10) + "`n"),
        [System.Text.UTF8Encoding]::new($false)
    )

    $ManifestPath = Join-Path $PackageDir "manifest.json"
    $Manifest = [ordered]@{
        version = $FixtureVersion
        binaries = @("webcodex", "webcodex-server", "webcodex-runner")
        artifacts = [ordered]@{
            "win32-x64" = [ordered]@{
                url = $ArtifactUrl
                sha256 = $Sha256
            }
        }
    }
    [System.IO.File]::WriteAllText(
        $ManifestPath,
        (($Manifest | ConvertTo-Json -Depth 6) + "`n"),
        [System.Text.UTF8Encoding]::new($false)
    )

    # The install is from a local tarball and the package has no npm
    # dependencies. Pin the registry to a non-routable endpoint so only the
    # WebCodex artifact download can use the network.
    $env:npm_config_registry = "http://127.0.0.1:9"
    Push-Location $PackageDir
    try {
        & npm pack --pack-destination $TempRoot --silent
        if ($LASTEXITCODE -ne 0) {
            throw "npm pack failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    $Tarball = Get-ChildItem -LiteralPath $TempRoot -Filter "yyjeqhc-webcodex-*.tgz" |
        Select-Object -First 1 -ExpandProperty FullName
    if (-not $Tarball) {
        throw "npm pack produced no tarball"
    }

    Write-Host "installing the x64 artifact through native ARM64 npm"
    & npm install --prefix $Prefix --no-audit --no-fund $Tarball
    if ($LASTEXITCODE -ne 0) {
        throw "npm install failed with exit code $LASTEXITCODE"
    }

    $Installed = Join-Path $Prefix "node_modules\@yyjeqhc\webcodex"
    $VendorBin = Join-Path $Installed "vendor\bin"
    foreach ($name in @("webcodex", "webcodex-server", "webcodex-runner")) {
        $binary = Join-Path $VendorBin "$name.exe"
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw "installed package is missing $name.exe"
        }
        if ((Get-PeMachine $binary) -ne 0x8664) {
            throw "installed $name.exe is not the expected x64 binary"
        }
    }

    $Cli = Join-Path $VendorBin "webcodex.exe"
    $Runner = Join-Path $VendorBin "webcodex-runner.exe"
    $CliVersion = & $Cli --version
    if ($LASTEXITCODE -ne 0 -or -not $CliVersion -or -not $CliVersion.StartsWith("webcodex $FixtureVersion ")) {
        throw "installed webcodex.exe produced unexpected output: $CliVersion"
    }
    & $Cli --help | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "installed webcodex.exe --help failed"
    }
    $RunnerVersion = & $Runner --version
    if ($LASTEXITCODE -ne 0 -or -not $RunnerVersion -or -not $RunnerVersion.StartsWith("webcodex-runner $FixtureVersion ")) {
        throw "installed webcodex-runner.exe produced unexpected output: $RunnerVersion"
    }

    $WrapperShim = Join-Path $Prefix "node_modules\.bin\webcodex.cmd"
    if (-not (Test-Path -LiteralPath $WrapperShim)) {
        throw "npm did not create the webcodex.cmd wrapper"
    }
    $WrapperVersion = & $WrapperShim --version
    if ($LASTEXITCODE -ne 0 -or -not $WrapperVersion -or -not $WrapperVersion.StartsWith("webcodex $FixtureVersion ")) {
        throw "npm wrapper produced unexpected output: $WrapperVersion"
    }

    Write-Host "Windows ARM64 npm -> x64 WebCodex compatibility smoke passed"
} finally {
    if ($null -eq $PreviousRegistry) {
        Remove-Item Env:npm_config_registry -ErrorAction SilentlyContinue
    } else {
        $env:npm_config_registry = $PreviousRegistry
    }
    if (Test-Path -LiteralPath $TempRoot) {
        Remove-Item -LiteralPath $TempRoot -Recurse -Force
    }
}
