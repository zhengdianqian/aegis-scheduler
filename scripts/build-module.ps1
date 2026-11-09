param(
    [string]$Version = "3.4.0",
    [string]$BuildDir = $env:TEMPO_ANDROID_TARGET_DIR,
    [string]$Apk,
    [string]$OutputDir
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $BuildDir) { $BuildDir = Join-Path $env:TEMP 'aegis-android-target' }
if (-not $OutputDir) { $OutputDir = Split-Path $root -Parent }
$bin = Join-Path $BuildDir 'aarch64-linux-android\release'
$stage = Join-Path $BuildDir 'module-stage'
$zip = Join-Path $OutputDir "Aegis-v$Version.zip"
if (-not $Apk) { $Apk = Join-Path $OutputDir "Aegis-v$Version.apk" }
if (-not (Test-Path $Apk)) { throw "Matching signed APK not found: $Apk" }
foreach ($required in @("$bin\Aegis", "$bin\tempoctl", "$root\webui\dist\index.html")) {
    if (-not (Test-Path $required)) { throw "Required build output not found: $required" }
}

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\bin", "$stage\scripts", "$stage\config", "$stage\webroot" | Out-Null

Copy-Item "$bin\Aegis", "$bin\tempoctl" "$stage\bin"
Copy-Item "$root\packaging\service.sh", "$root\packaging\uninstall.sh", "$root\packaging\customize.sh" $stage
Copy-Item "$root\packaging\module.properties" "$stage\module.prop"
Copy-Item "$root\LICENSE" "$stage\LICENSE"
Copy-Item "$root\packaging\shield.sh", "$root\packaging\charge_resume.sh", "$root\packaging\watchdog.sh", "$root\packaging\cpufreq_backup.sh", "$root\packaging\restore.sh", "$root\packaging\profile-gen.sh" "$stage\scripts"
Copy-Item "$root\packaging\config.toml", "$root\packaging\fas.json" "$stage\config"
# v3 derives the cluster layout from sysfs at boot; no SoC-specific profile is
# shipped, so an unsupported device degrades to read-only instead of writing a
# wrong frequency table.
Copy-Item "$root\packaging\icon.png" "$stage\webroot\icon.png"
if (Test-Path "$root\packaging\META-INF") { Copy-Item "$root\packaging\META-INF" $stage -Recurse }
Copy-Item "$root\webui\dist\*" "$stage\webroot" -Recurse

# Bundle the matching signed APK under app/.
New-Item -ItemType Directory -Force "$stage\app" | Out-Null
Copy-Item $Apk "$stage\app\AegisManager.apk"

Remove-Item $zip -Force -ErrorAction SilentlyContinue
# Pack with Python using the reproducible attribute layout described in
# pack_zip.py: create_system=0, files 100644, dirs 40755, explicit dir entries.
$py = $env:PYTHON
$pyArgs = @()
if (-not $py) {
    $launcher = Get-Command py.exe -ErrorAction SilentlyContinue
    if ($launcher) {
        $py = $launcher.Source
        $pyArgs = @('-3')
    } else {
        $py = 'python'
    }
}
& $py @pyArgs "$PSScriptRoot\pack_zip.py" $stage $zip
if ($LASTEXITCODE -ne 0) { throw "zip packing failed" }
Write-Output "built $zip"
