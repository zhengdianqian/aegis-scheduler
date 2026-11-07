# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
# Builds the Aegis Android app: refreshes webui dist, syncs assets, runs Gradle, copies the APK.

$ErrorActionPreference = 'Stop'

$appRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $appRoot
$webuiRoot = Join-Path $repoRoot 'webui'
$webuiDist = Join-Path $webuiRoot 'dist'
$assets = Join-Path $appRoot 'app\src\main\assets\webui'
$apkOut = Join-Path $appRoot 'app\build\outputs\apk\debug\app-debug.apk'
$deliveryDir = 'D:\dev\tempo-build-android\tempo-app'
$deliveryApk = Join-Path $deliveryDir 'Aegis-v2.1.1.apk'

# 1. Regenerate the static WebUI bundle from sources.
& (Join-Path $webuiRoot 'build.ps1')

# 2. Mirror the bundle into Android assets.
if (-not (Test-Path $webuiDist)) { throw "missing webui dist: $webuiDist" }
if (Test-Path $assets) { Remove-Item -Path $assets -Recurse -Force }
New-Item -ItemType Directory -Path $assets | Out-Null
Get-ChildItem -Path $webuiDist -File | ForEach-Object {
  Copy-Item -Path $_.FullName -Destination (Join-Path $assets $_.Name) -Force
}

# 3. Locate Gradle: $env:GRADLE wins, then the local wrapper install, then PATH.
$gradle = $env:GRADLE
if (-not $gradle) {
  $candidate = Join-Path $env:USERPROFILE '.gradle\wrapper\dists\gradle-9.5.0-bin\bvnork1r7n8i6kp5cnkibsc9q\gradle-9.5.0\bin\gradle.bat'
  if (Test-Path $candidate) { $gradle = $candidate }
}
if (-not $gradle) { $gradle = 'gradle' }

# 4. Build. Prefer the offline cache, retry online if that fails.
Push-Location $appRoot
try {
  & $gradle 'assembleDebug' '--offline' '--console=plain'
  if ($LASTEXITCODE -ne 0) {
    Write-Host 'offline build failed, retrying with network access...'
    & $gradle 'assembleDebug' '--console=plain'
    if ($LASTEXITCODE -ne 0) { throw "gradle build failed: $LASTEXITCODE" }
  }
} finally {
  Pop-Location
}

if (-not (Test-Path $apkOut)) { throw "missing apk: $apkOut" }

# 5. Publish the installable APK.
if (-not (Test-Path $deliveryDir)) { New-Item -ItemType Directory -Path $deliveryDir | Out-Null }
Copy-Item -Path $apkOut -Destination $deliveryApk -Force

Write-Host ''
Write-Host "APK ready: $deliveryApk"
Get-Item $deliveryApk | Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize
