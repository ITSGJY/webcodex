# Opt-in: reuse a warm Cargo target across worktrees of the SAME Git repository.
# This is a developer validation helper, not a Runner execution/default policy.
[CmdletBinding()]
param(
    [string]$WorktreePath = (Get-Location).Path,
    [Parameter(Mandatory = $true)]
    [ValidateCount(1, 256)]
    [string[]]$CargoArgs,
    [string]$CargoExecutable = 'cargo',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    throw "Shared Cargo worktree cache: $Message"
}

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Fail 'Git is required to prove a common worktree repository.'
}
if (-not (Test-Path -LiteralPath $WorktreePath -PathType Container)) {
    Fail 'WorktreePath must name an existing directory.'
}
if (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    Fail 'CARGO_TARGET_DIR is already set; the existing explicit choice must not be overwritten.'
}
foreach ($arg in $CargoArgs) {
    if ($arg -eq '--target-dir' -or $arg.StartsWith('--target-dir=') -or
        $arg -eq '--manifest-path' -or $arg.StartsWith('--manifest-path=') -or
        $arg -eq '--config' -or $arg.StartsWith('--config=') -or
        $arg.StartsWith('-C')) {
        Fail 'Do not override target-dir, Cargo config or the selected workspace.'
    }
}

# git-common-dir is the SAME directory for the primary checkout and its
# linked worktrees, unlike their distinct git-dir paths and current HEADs.
$checkout = ([string](& git -C $WorktreePath rev-parse --show-toplevel 2>$null)).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($checkout)) {
    Fail 'WorktreePath is not a Git checkout.'
}
$common = ([string](& git -C $WorktreePath rev-parse --path-format=absolute --git-common-dir 2>$null)).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($common)) {
    Fail 'Cannot resolve the Git common directory.'
}
$checkout = [IO.Path]::GetFullPath($checkout)
$common = [IO.Path]::GetFullPath($common)

# A helper shipped by repository A must never redirect a command in repository
# B into B's build output: link the requested worktree to the script's own
# authoritative Git common directory rather than trusting the caller's path.
$helperCheckout = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$helperCommon = ([string](& git -C $helperCheckout rev-parse --path-format=absolute --git-common-dir 2>$null)).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($helperCommon) -or
    [IO.Path]::GetFullPath($helperCommon) -ne $common) {
    Fail 'WorktreePath belongs to a different Git repository than this helper.'
}
if ((Split-Path -Leaf $common) -cne '.git') {
    Fail 'Only ordinary Git repositories with a .git common directory are supported.'
}
$primary = [IO.Path]::GetFullPath((Split-Path -Parent $common))
if (-not (Test-Path -LiteralPath (Join-Path $checkout 'Cargo.toml') -PathType Leaf) -or
    -not (Test-Path -LiteralPath (Join-Path $primary 'Cargo.toml') -PathType Leaf)) {
    Fail 'Both linked worktree and primary checkout must contain a root Cargo.toml.'
}
$sharedTarget = [IO.Path]::GetFullPath((Join-Path $primary 'target'))

if ($DryRun) {
    # No target directory creation, Git changes or Cargo invocation.
    [pscustomobject]@{
        worktree = $checkout
        primary_checkout = $primary
        shared_target = $sharedTarget
        cargo = $CargoExecutable
        cargo_args = @($CargoArgs)
        compilation_started = $false
    } | ConvertTo-Json -Depth 4 -Compress
    return
}

if (-not (Get-Command $CargoExecutable -ErrorAction SilentlyContinue)) {
    Fail 'Requested Cargo executable is unavailable.'
}
$priorLocation = Get-Location
$priorTarget = $env:CARGO_TARGET_DIR
try {
    # Cargo itself owns build-cache locking when multiple worktrees build.
    # Source snapshots, Git revisions and approval remain independent.
    $env:CARGO_TARGET_DIR = $sharedTarget
    Set-Location -LiteralPath $checkout
    & $CargoExecutable @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        Fail "Cargo command failed with exit code $LASTEXITCODE."
    }
} finally {
    $env:CARGO_TARGET_DIR = $priorTarget
    Set-Location -LiteralPath $priorLocation.Path
}
