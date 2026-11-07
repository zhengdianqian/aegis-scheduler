# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
# Copies webui/src to webui/dist. No bundler, no network, no minification.

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$src = Join-Path $root 'src'
$dst = Join-Path $root 'dist'

if (-not (Test-Path $src)) { throw "missing source directory: $src" }

if (Test-Path $dst) { Remove-Item -Path $dst -Recurse -Force }
New-Item -ItemType Directory -Path $dst | Out-Null

Get-ChildItem -Path $src -File | ForEach-Object {
  Copy-Item -Path $_.FullName -Destination (Join-Path $dst $_.Name) -Force
}

Write-Host "webui dist ready: $dst"
Get-ChildItem -Path $dst -File | Select-Object Name, Length | Format-Table -AutoSize
