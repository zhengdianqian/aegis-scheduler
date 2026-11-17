# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
#
# Cross-compiles tempod / tempoctl for arm64 Android.
# All output paths must stay pure ASCII: the workspace lives under a CJK path.

param(
    [switch]$Release,
    [string]$Ndk = $env:ANDROID_NDK_HOME,
    [int]$ApiLevel = 26,
    # 探针源未改动且 probe/dist/tempo.bpf.o 已就位时可跳过（本机无 nightly/bpf-linker）。
    [switch]$SkipProbe
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$target = $env:TEMPO_ANDROID_TARGET_DIR
if (-not $target) { $target = "D:\dev\tempo-build-android" }
if ($target -match "[^\x00-\x7F]") {
    throw "TEMPO_ANDROID_TARGET_DIR must be a pure ASCII path, got: $target"
}

if (-not $Ndk) {
    $candidate = "D:\dev\ndk\android-ndk-r29"
    if (Test-Path $candidate) { $Ndk = $candidate }
}
if (-not $Ndk -or -not (Test-Path $Ndk)) {
    throw "Android NDK not found; set ANDROID_NDK_HOME"
}

$toolchain = Join-Path $Ndk "toolchains\llvm\prebuilt\windows-x86_64\bin"
$linker = Join-Path $toolchain "aarch64-linux-android$ApiLevel-clang.cmd"
if (-not (Test-Path $linker)) {
    throw "linker wrapper not found: $linker"
}

$cargo = $env:CARGO
if (-not $cargo -and (Test-Path "D:\dev\cargo\bin\cargo.exe")) {
    $cargo = "D:\dev\cargo\bin\cargo.exe"
}
if (-not $cargo) { $cargo = "cargo" }

$env:CARGO_TARGET_DIR = $target
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $linker
$env:CC_aarch64_linux_android = $linker
$env:AR_aarch64_linux_android = Join-Path $toolchain "llvm-ar.exe"

Write-Host "CARGO_TARGET_DIR=$target"
Write-Host "linker=$linker"

# tempod embeds the probe object, so build it first (unless already provided).
if (-not $SkipProbe) {
    & (Join-Path $PSScriptRoot "build-probe.ps1")
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} elseif (-not (Test-Path (Join-Path $root "probe\dist\tempo.bpf.o"))) {
    throw "-SkipProbe requires an existing probe/dist/tempo.bpf.o"
}

Push-Location $root
try {
    $profileArgs = @()
    if ($Release) { $profileArgs = @("--release") }
    & $cargo build --target aarch64-linux-android --workspace @profileArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
