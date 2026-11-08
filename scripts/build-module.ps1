param([string]$Version = "2.0.0")

$root = Split-Path $PSScriptRoot -Parent
$bin = "D:\dev\tempo-build-android\aarch64-linux-android\release"
$stage = "D:\dev\tempo-build-android\module-stage"
$zip = "D:\dev\tempo-build-android\Aegis-v$Version.zip"

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\bin", "$stage\scripts", "$stage\config", "$stage\webroot" | Out-Null

Copy-Item "$bin\Aegis", "$bin\tempoctl" "$stage\bin"
Copy-Item "$root\packaging\service.sh", "$root\packaging\uninstall.sh", "$root\packaging\customize.sh" $stage
Copy-Item "$root\packaging\module.properties" "$stage\module.prop"
Copy-Item "$root\packaging\shield.sh", "$root\packaging\charge_resume.sh", "$root\packaging\watchdog.sh", "$root\packaging\cpufreq_backup.sh", "$root\packaging\restore.sh", "$root\packaging\mask_min.sh" "$stage\scripts"
Copy-Item "$root\packaging\config.toml", "$root\packaging\fas.json" "$stage\config"
Copy-Item "$root\profiles\babylon.toml" "$stage\config\babylon.toml"
Copy-Item "$root\packaging\icon.png" "$stage\icon.png"
Copy-Item "$root\packaging\icon.png" "$stage\webroot\icon.png"
if (Test-Path "$root\LICENSE") { Copy-Item "$root\LICENSE" $stage }
if (Test-Path "$root\README.md") { Copy-Item "$root\README.md" $stage }
Copy-Item "$root\webui\dist\*" "$stage\webroot" -Recurse

# Bundle the matching APK so the module zip is self-contained.
$apk = 'D:\dev\tempo-build-android\tempo-app\Aegis-v{0}.apk' -f $Version
if (Test-Path $apk) { Copy-Item $apk $stage }

Remove-Item $zip -Force -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$stream = [System.IO.File]::Create($zip)
$archive = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create)
Get-ChildItem $stage -Recurse -File | ForEach-Object {
    $relative = $_.FullName.Substring($stage.Length + 1).Replace('\', '/')
    $entry = $archive.CreateEntry($relative, [System.IO.Compression.CompressionLevel]::Optimal)
    $target = $entry.Open()
    $bytes = [System.IO.File]::ReadAllBytes($_.FullName)
    $target.Write($bytes, 0, $bytes.Length)
    $target.Dispose()
}
$archive.Dispose()
$stream.Dispose()
Write-Output "built $zip"
