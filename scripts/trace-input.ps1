# Runs the freshly built dvz with its input trace on, so an IME problem can be
# read back from the events the terminal actually delivered. The log is
# truncated first: a trace worth reading is one reproduction, not a pile.
$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot '..\target\release\dvz.exe' | Resolve-Path
$log = Join-Path $env:TEMP 'dvz-input.log'

if (Test-Path $log) { Remove-Item $log -Force }
$env:DVZ_INPUT_LOG = $log

Write-Host "입력 기록: $log" -ForegroundColor Cyan
Write-Host '재현한 뒤 dvz를 종료하세요.' -ForegroundColor Cyan

& $exe @args
