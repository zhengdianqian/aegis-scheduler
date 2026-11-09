# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
# Builds the Aegis Android app: refreshes webui dist, syncs assets, runs Gradle release build,
# verifies the release signature, then publishes the signed APK.

$ErrorActionPreference = 'Stop'

$appRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $appRoot
$webuiRoot = Join-Path $repoRoot 'webui'
$webuiDist = Join-Path $webuiRoot 'dist'
$assets = Join-Path $appRoot 'app\src\main\assets\webui'
$apkOut = Join-Path $appRoot 'app\build\outputs\apk\release\app-release.apk'
$unsignedApk = Join-Path $appRoot 'app\build\outputs\apk\release\app-release-unsigned.apk'

# 版本号从模块 Gradle 配置读取，保证产物文件名与版本一致。
$moduleGradle = Join-Path $appRoot 'app\build.gradle.kts'
$versionMatch = Select-String -Path $moduleGradle -Pattern 'versionName\s*=\s*"([^"]+)"'
if (-not $versionMatch) { throw "cannot read versionName from $moduleGradle" }
$versionName = $versionMatch.Matches[0].Groups[1].Value

$deliveryDir = $env:AEGIS_APK_OUTPUT_DIR
if (-not $deliveryDir) { $deliveryDir = [System.IO.Path]::GetTempPath() }
$deliveryApk = Join-Path $deliveryDir "Aegis-v$versionName.apk"

# 1. Regenerate the static WebUI bundle from sources.
& (Join-Path $webuiRoot 'build.ps1')

# 2. Mirror the bundle into Android assets and prove both trees are byte-identical.
if (-not (Test-Path $webuiDist)) { throw "missing webui dist: $webuiDist" }
if (Test-Path $assets) { Remove-Item -Path $assets -Recurse -Force }
New-Item -ItemType Directory -Path $assets | Out-Null
Get-ChildItem -Path $webuiDist -File | ForEach-Object {
  Copy-Item -Path $_.FullName -Destination (Join-Path $assets $_.Name) -Force
}

$distFiles = Get-ChildItem -Path $webuiDist -File
$assetFiles = Get-ChildItem -Path $assets -File
if ($distFiles.Count -ne $assetFiles.Count) {
  throw "assets/webui 文件数与 dist 不一致：$($assetFiles.Count) vs $($distFiles.Count)"
}
foreach ($file in $distFiles) {
  $assetPath = Join-Path $assets $file.Name
  if (-not (Test-Path $assetPath)) { throw "assets/webui 缺少文件：$($file.Name)" }
  $distHash = (Get-FileHash -Path $file.FullName -Algorithm SHA256).Hash
  $assetHash = (Get-FileHash -Path $assetPath -Algorithm SHA256).Hash
  if ($distHash -ne $assetHash) {
    throw "assets/webui/$($file.Name) 与 dist 哈希不一致"
  }
}

# 3. Locate Gradle: $env:GRADLE wins, then the local wrapper install, then PATH.
$gradle = $env:GRADLE
if (-not $gradle) {
  $candidate = Join-Path $env:USERPROFILE '.gradle\wrapper\dists\gradle-9.5.0-bin\bvnork1r7n8i6kp5cnkibsc9q\gradle-9.5.0\bin\gradle.bat'
  if (Test-Path $candidate) { $gradle = $candidate }
}
if (-not $gradle) { $gradle = 'gradle' }

# 4. Build release only. Never fall back to a debug or unsigned package: if the offline
#    cache is missing and the network is unavailable, fail loudly with the real reason.
Push-Location $appRoot
try {
  & $gradle 'assembleRelease' '--offline' '--console=plain'
  if ($LASTEXITCODE -ne 0) {
    Write-Host 'offline build failed (Gradle cache incomplete?), retrying with network access...'
    & $gradle 'assembleRelease' '--console=plain'
    if ($LASTEXITCODE -ne 0) {
      throw "gradle assembleRelease failed (exit $LASTEXITCODE)。请按上方 Gradle 报错处理（签名配置 / 依赖缓存 / 网络之一）；本脚本不会回退到 debug 或未签名包。"
    }
  }
} finally {
  Pop-Location
}

if (-not (Test-Path $apkOut)) {
  if (Test-Path $unsignedApk) {
    throw "release APK 未签名（只找到 app-release-unsigned.apk）。请检查 app/keystore/keystore.properties 与 app/app/build.gradle.kts 的 signingConfigs。"
  }
  throw "missing signed release apk: $apkOut"
}
if ((Get-Item $apkOut).Length -eq 0) { throw "signed release apk is empty: $apkOut" }

# 5. Verify the signature with the SDK's apksigner before publishing.
$sdkDir = $env:ANDROID_HOME
if (-not $sdkDir) {
  $localProps = Join-Path $appRoot 'local.properties'
  if (Test-Path $localProps) {
    $sdkLine = Select-String -Path $localProps -Pattern '^sdk\.dir=(.+)$'
    if ($sdkLine) { $sdkDir = $sdkLine.Matches[0].Groups[1].Value.Trim().Replace('\\', '\') }
  }
}
if (-not $sdkDir) { throw "无法定位 Android SDK：请设置 ANDROID_HOME 或写入 app/local.properties 的 sdk.dir" }

$buildTools = $null
foreach ($candidateVersion in @('36.0.0', '35.0.0')) {
  $candidate = Join-Path $sdkDir "build-tools\$candidateVersion"
  if (Test-Path (Join-Path $candidate 'apksigner.bat')) { $buildTools = $candidate; break }
}
if (-not $buildTools) { throw "未在 $sdkDir\build-tools 下找到 apksigner（需要 35.0.0 或 36.0.0）" }

$apksigner = Join-Path $buildTools 'apksigner.bat'
$aapt2 = Join-Path $buildTools 'aapt2.exe'

$verifyOutput = & $apksigner verify --verbose --print-certs $apkOut 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { throw "apksigner verify 失败：`n$verifyOutput" }

$certDn = ([regex]::Match($verifyOutput, 'Signer #1 certificate DN: (.+)')).Groups[1].Value.Trim()
$certSha256 = ([regex]::Match($verifyOutput, 'Signer #1 certificate SHA-256 digest: ([0-9a-f]+)')).Groups[1].Value.Trim()
if (-not $certDn) { throw "apksigner 未返回证书 DN：`n$verifyOutput" }
if ($certDn -match 'CN=Android Debug' -or $certSha256 -eq 'a3ef110785845814615dfa224e0e3e79f43fe1427d854463296e3bd9eccd0e4d') {
  throw "产物使用了 debug 证书，拒绝发布：$certDn"
}
if ($certDn -notmatch 'CN=Aegis') {
  throw "产物证书不是 Aegis 正式证书，拒绝发布：$certDn"
}

$scheme = @{}
foreach ($line in ($verifyOutput -split "`r?`n")) {
  if ($line -match 'Verified using (v[0-9.]+) scheme.*: (true|false)') {
    $scheme[$Matches[1]] = $Matches[2]
  }
}
if (-not ($scheme['v2'] -eq 'true' -or $scheme['v3'] -eq 'true')) {
  throw "产物没有 v2/v3 签名，拒绝发布：`n$verifyOutput"
}

$badging = & $aapt2 dump badging $apkOut 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { throw "aapt2 dump badging 失败：`n$badging" }
if ($badging -match 'application-debuggable') {
  throw "产物 debuggable=true，拒绝发布 release 包"
}
$packageLine = ([regex]::Match($badging, "package: name='([^']+)' versionCode='([^']+)' versionName='([^']+)'")).Groups
$packageName = $packageLine[1].Value
$versionCode = $packageLine[2].Value
$apkVersionName = $packageLine[3].Value
if ($packageName -ne 'com.aegis.scheduler') {
  throw "APK 包名 $packageName 与 com.aegis.scheduler 不一致"
}
$codeMatch = Select-String -Path $moduleGradle -Pattern 'versionCode\s*=\s*(\d+)'
$expectedVersionCode = if ($codeMatch) { $codeMatch.Matches[0].Groups[1].Value } else { '' }
if (-not $expectedVersionCode -or $versionCode -ne $expectedVersionCode) {
  throw "APK versionCode=$versionCode 与 Gradle 配置 $expectedVersionCode 不一致"
}
if ($apkVersionName -ne $versionName) {
  throw "APK versionName=$apkVersionName 与 Gradle 配置 $versionName 不一致"
}

$size = (Get-Item $apkOut).Length
$sha256 = (Get-FileHash -Path $apkOut -Algorithm SHA256).Hash

# 6. Publish the verified installable APK.
if (-not (Test-Path $deliveryDir)) { New-Item -ItemType Directory -Path $deliveryDir | Out-Null }
Copy-Item -Path $apkOut -Destination $deliveryApk -Force

$deliverySize = (Get-Item $deliveryApk).Length
$deliverySha256 = (Get-FileHash -Path $deliveryApk -Algorithm SHA256).Hash

Write-Host ''
Write-Host '=== APK 签名与产物报告 ==='
Write-Host "APK: $deliveryApk"
Write-Host "包名: $packageName"
Write-Host "版本: versionName=$apkVersionName versionCode=$versionCode"
Write-Host ("签名方案: v1={0} v2={1} v3={2} v4={3}" -f $scheme['v1'], $scheme['v2'], $scheme['v3'], $scheme['v4'])
Write-Host "证书 DN: $certDn"
Write-Host "证书 SHA-256: $certSha256"
Write-Host "大小: $deliverySize B"
Write-Host "SHA256: $deliverySha256"
Write-Host "debuggable: false"
