# No-build smoke for the opt-in shared Cargo target helper.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$checkout = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$helper = Join-Path $checkout 'scripts\cargo_shared_worktree_target.ps1'
$common = ([string](& git -C $checkout rev-parse --path-format=absolute --git-common-dir)).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Cannot find Git common directory for test.' }
$primary = [IO.Path]::GetFullPath((Split-Path -Parent $common))
$target = [IO.Path]::GetFullPath((Join-Path $primary 'target'))
$argsVector = @('test', '--locked', '-p', 'webcodex', '--lib', '--', '--exact', 'filter with spaces')

function Assert([bool]$Check, [string]$Message) {
    if (-not $Check) { throw "Shared target regression: $Message" }
}

$originalTarget = $env:CARGO_TARGET_DIR
$originalLocation = (Get-Location).Path
$originalProbe = $env:WEBCODEX_CARGO_TARGET_PROBE
$originalFail = $env:WEBCODEX_CARGO_TARGET_PROBE_FAIL
$temp = Join-Path ([IO.Path]::GetTempPath()) ('wc-cargo-cache-smoke-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null

try {
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue

    $linked = & $helper -WorktreePath $checkout -CargoArgs $argsVector -DryRun | ConvertFrom-Json
    $root = & $helper -WorktreePath $primary -CargoArgs $argsVector -DryRun | ConvertFrom-Json
    Assert ($linked.shared_target -eq $target) 'linked worktree cache path changed'
    Assert ($root.shared_target -eq $target) 'primary checkout must share the same target'
    Assert ($linked.worktree -eq $checkout) 'linked worktree path changed'
    Assert ($linked.compilation_started -eq $false) 'DryRun must never compile'
    Assert ((-not (Test-Path (Join-Path $temp 'fake-cargo-out.json')))) 'DryRun emitted a Cargo record'

    $fake = Join-Path $temp 'fake-cargo.ps1'
    $recordFile = Join-Path $temp 'fake-cargo-out.json'
    $fakeLines = @(
        '[pscustomobject]@{'
        '    worktree = (Get-Location).Path'
        '    target = $env:CARGO_TARGET_DIR'
        '    argv = @($args)'
        '} | ConvertTo-Json -Depth 4 -Compress | Set-Content -LiteralPath $env:WEBCODEX_CARGO_TARGET_PROBE -Encoding utf8NoBOM'
        'if ($env:WEBCODEX_CARGO_TARGET_PROBE_FAIL -eq "1") { throw "synthetic fake Cargo failure" }'
    )
    $fakeLines | Set-Content -LiteralPath $fake -Encoding utf8NoBOM
    $env:WEBCODEX_CARGO_TARGET_PROBE = $recordFile

    & $helper -WorktreePath $checkout -CargoExecutable $fake -CargoArgs $argsVector
    $captured = Get-Content -LiteralPath $recordFile -Raw | ConvertFrom-Json
    Assert ($captured.worktree -eq $checkout) 'Cargo executed outside linked checkout'
    Assert ($captured.target -eq $target) 'Cargo did not inherit the shared cache path'
    Assert (($captured.argv | ConvertTo-Json -Compress) -eq ($argsVector | ConvertTo-Json -Compress)) 'Cargo arguments were modified'
    Assert ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) 'Cargo target environment leaked after success'
    Assert ((Get-Location).Path -eq $originalLocation) 'PowerShell cwd leaked after success'

    $env:WEBCODEX_CARGO_TARGET_PROBE_FAIL = '1'
    $failed = $false
    try { & $helper -WorktreePath $checkout -CargoExecutable $fake -CargoArgs $argsVector }
    catch { $failed = $true }
    Assert $failed 'fake Cargo failure must fail closed'
    Assert ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) 'Cargo target leaked after failure'
    Assert ((Get-Location).Path -eq $originalLocation) 'PowerShell cwd leaked after failure'
    Remove-Item Env:WEBCODEX_CARGO_TARGET_PROBE_FAIL

    foreach ($override in @('--target-dir=other', '--manifest-path=other.toml',
                             '--config=build.target-dir="other"', '-C=other', '-Cother')) {
        $rejected = $false
        try { & $helper -WorktreePath $checkout -CargoArgs @('test', $override) -DryRun | Out-Null }
        catch { $rejected = $true }
        Assert $rejected "Cargo args override was accepted: $override"
    }

    $env:CARGO_TARGET_DIR = Join-Path $temp 'preselected'
    $rejected = $false
    try { & $helper -WorktreePath $checkout -CargoArgs $argsVector -DryRun | Out-Null }
    catch { $rejected = $true }
    Assert $rejected 'preselected CARGO_TARGET_DIR must not be overwritten'
    Assert ($env:CARGO_TARGET_DIR -eq (Join-Path $temp 'preselected')) 'preselected target was modified'
    Remove-Item Env:CARGO_TARGET_DIR

    $outsider = Join-Path $temp 'not-a-repo'
    New-Item -ItemType Directory -Path $outsider | Out-Null
    $rejected = $false
    try { & $helper -WorktreePath $outsider -CargoArgs $argsVector -DryRun | Out-Null }
    catch { $rejected = $true }
    Assert $rejected 'non-Git directory was accepted'

    $foreign = Join-Path $temp 'foreign-git'
    New-Item -ItemType Directory -Path $foreign | Out-Null
    & git -C $foreign init --quiet
    Assert ($LASTEXITCODE -eq 0) 'foreign Git repository fixture could not be initialized'
    Set-Content -LiteralPath (Join-Path $foreign 'Cargo.toml') -Value '[workspace]' -Encoding utf8NoBOM
    $rejected = $false
    try { & $helper -WorktreePath $foreign -CargoArgs $argsVector -DryRun | Out-Null }
    catch { $rejected = $true }
    Assert $rejected 'another Git repository with Cargo.toml was accepted'

    Write-Output 'PASS: same-Git worktrees share warm target, DryRun never compiles, args forwarded exactly'
    Write-Output 'PASS: cargo error and preexisting target env do not leak, foreign checkout and override rejected'
} finally {
    $env:CARGO_TARGET_DIR = $originalTarget
    $env:WEBCODEX_CARGO_TARGET_PROBE = $originalProbe
    $env:WEBCODEX_CARGO_TARGET_PROBE_FAIL = $originalFail
    Set-Location -LiteralPath $originalLocation
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    $global:LASTEXITCODE = 0
}
