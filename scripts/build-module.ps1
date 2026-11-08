param([string]$Version = "3.0")

$root = Split-Path $PSScriptRoot -Parent
$bin = "D:\dev\tempo-build-android\aarch64-linux-android\release"
$stage = "D:\dev\tempo-build-android\module-stage"
$zip = "D:\dev\tempo-build-android\Aegis-v$Version.zip"

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\bin", "$stage\scripts", "$stage\config", "$stage\webroot" | Out-Null

Copy-Item "$bin\Aegis", "$bin\tempoctl" "$stage\bin"
Copy-Item "$root\packaging\service.sh", "$root\packaging\uninstall.sh", "$root\packaging\customize.sh" $stage
Copy-Item "$root\packaging\module.properties" "$stage\module.prop"
Copy-Item "$root\packaging\shield.sh", "$root\packaging\charge_resume.sh", "$root\packaging\watchdog.sh", "$root\packaging\cpufreq_backup.sh", "$root\packaging\restore.sh", "$root\packaging\profile-gen.sh" "$stage\scripts"
Copy-Item "$root\packaging\config.toml", "$root\packaging\fas.json" "$stage\config"
# v3 derives the cluster layout from sysfs at boot; no SoC-specific profile is
# shipped, so an unsupported device degrades to read-only instead of writing a
# wrong frequency table.
Copy-Item "$root\packaging\icon.png" "$stage\webroot\icon.png"
if (Test-Path "$root\packaging\META-INF") { Copy-Item "$root\packaging\META-INF" $stage -Recurse }
Copy-Item "$root\webui\dist\*" "$stage\webroot" -Recurse

# Bundle the matching signed APK under app/.
$apk = 'D:\dev\tempo-build-android\tempo-app\Aegis-v{0}.apk' -f $Version
if (Test-Path $apk) {
    New-Item -ItemType Directory -Force "$stage\app" | Out-Null
    Copy-Item $apk "$stage\app\AegisManager.apk"
}

Remove-Item $zip -Force -ErrorAction SilentlyContinue
# Pack with Python using the reproducible attribute layout described in
# pack_zip.py: create_system=0, files 100644, dirs 40755, explicit dir entries.
$py = 'C:\Windows\py.exe'
if (-not (Test-Path $py)) { $py = 'python' }
& $py -3 "$PSScriptRoot\pack_zip.py" $stage $zip
if ($LASTEXITCODE -ne 0) { throw "zip packing failed" }
Write-Output "built $zip"
