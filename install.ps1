[CmdletBinding()]
param(
    [string] $BinDir,
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

if ([string]::IsNullOrWhiteSpace($BinDir)) {
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        $BinDir = Join-Path $env:LOCALAPPDATA 'workstats\bin'
    } elseif (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $BinDir = Join-Path $env:USERPROFILE '.local\bin'
    } else {
        throw 'install.ps1: cannot determine an install directory; pass -BinDir'
    }
}

$cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
if ($null -eq $cargo) {
    throw 'install.ps1: cargo not found; install Rust from https://rustup.rs'
}

& $cargo.Source build --release --locked --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) {
    throw "install.ps1: cargo build failed with exit code $LASTEXITCODE"
}

$source = Join-Path $root 'target\release\workstats.exe'
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw 'install.ps1: Rust build did not produce workstats.exe'
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$BinDir = (Resolve-Path -LiteralPath $BinDir).Path

$target = Join-Path $BinDir 'workstats.exe'
if (Test-Path -LiteralPath $target) {
    if (-not $Force) {
        throw "install.ps1: refusing to replace $target; rerun with -Force to preserve it as .before-workstats"
    }
    $backup = "$target.before-workstats"
    $suffix = 1
    while (Test-Path -LiteralPath $backup) {
        $backup = "$target.before-workstats.$suffix"
        $suffix += 1
    }
    Move-Item -LiteralPath $target -Destination $backup
}
Copy-Item -LiteralPath $source -Destination $target

Write-Host "Installed workstats.exe in $BinDir"
Write-Host 'Add that directory to PATH if it is not already there.'
