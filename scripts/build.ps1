# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文

param(
    [switch]$Test,
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$target = $env:TEMPO_TARGET_DIR
if (-not $target) { $target = "D:\dev\tempo-build" }
if ($target -match "[^\x00-\x7F]") {
    throw "TEMPO_TARGET_DIR must be a pure ASCII path, got: $target"
}
$env:CARGO_TARGET_DIR = $target
Write-Host "CARGO_TARGET_DIR=$target"

$cargo = $env:CARGO
if (-not $cargo) {
    if (Test-Path "D:\dev\cargo\bin\cargo.exe") {
        $cargo = "D:\dev\cargo\bin\cargo.exe"
    } else {
        $cargo = "cargo"
    }
}

Push-Location $root
try {
    if ($Test) {
        & $cargo test --workspace
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    if ($Release) {
        & $cargo build --release --workspace
    } else {
        & $cargo build --workspace
    }
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
