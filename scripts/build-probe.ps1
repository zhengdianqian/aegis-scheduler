# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
#
# Builds the BPF probe object and publishes it to probe/dist/tempo.bpf.o.
# tempod embeds that object at compile time, so this must run before tempod is
# built (scripts/build-android.ps1 does it automatically).

param(
    [string]$BpfLinker = "D:\dev\bpflinker",
    [string]$TargetDir = $env:TEMPO_PROBE_TARGET_DIR
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$probe = Join-Path $root "probe"

if (-not $TargetDir) { $TargetDir = "D:\dev\tempo-build-probe" }
if ($TargetDir -match "[^\x00-\x7F]") {
    throw "TEMPO_PROBE_TARGET_DIR must be a pure ASCII path, got: $TargetDir"
}

if (-not (Test-Path (Join-Path $BpfLinker "bpf-linker.exe"))) {
    throw "bpf-linker.exe not found in $BpfLinker"
}
$env:PATH = "$BpfLinker;$env:PATH"

# This script is also invoked from build-android.ps1, which relies on its own
# CARGO_TARGET_DIR for the Android artifacts; restore it on the way out.
$previousTargetDir = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = $TargetDir

$cargo = $env:CARGO
if (-not $cargo -and (Test-Path "D:\dev\cargo\bin\cargo.exe")) {
    $cargo = "D:\dev\cargo\bin\cargo.exe"
}
if (-not $cargo) { $cargo = "cargo" }

Push-Location $probe
try {
    & $cargo build --release
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}

$built = Join-Path $TargetDir "bpfel-unknown-none\release\tempo-probe"
if (-not (Test-Path $built)) { throw "probe object not found at $built" }

$dist = Join-Path $probe "dist"
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item $built (Join-Path $dist "tempo.bpf.o") -Force

if ($previousTargetDir) {
    $env:CARGO_TARGET_DIR = $previousTargetDir
} else {
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
}

Write-Host "probe object -> $dist\tempo.bpf.o"
