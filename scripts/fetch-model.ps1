# Скачивает модель распознавания GigaAM v2 (sherpa-onnx, ~160 МБ архив) рядом с exe.
# Запуск:  powershell -ExecutionPolicy Bypass -File scripts\fetch-model.ps1 [-Dest папка_с_exe]
param([string]$Dest = (Join-Path $PSScriptRoot "..\target\release"))

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$name = "sherpa-onnx-nemo-ctc-giga-am-v2-russian-2025-04-19"
$url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$name.tar.bz2"

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
$Dest = (Resolve-Path $Dest).Path
$target = Join-Path $Dest "models\gigaam-v2-ru"
if (Test-Path (Join-Path $target "model.int8.onnx")) {
    Write-Host "Модель уже есть: $target"
    exit 0
}

$tmp = Join-Path $env:TEMP "solonka-model"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$archive = Join-Path $tmp "model.tar.bz2"
Write-Host "Скачиваю $name ..."
Invoke-WebRequest $url -OutFile $archive
tar -xjf $archive -C $tmp
if ($LASTEXITCODE -ne 0) { throw "tar не смог распаковать архив" }

New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item (Join-Path $tmp "$name\model.int8.onnx"), (Join-Path $tmp "$name\tokens.txt") $target -Force
Remove-Item -Recurse -Force $tmp
Write-Host "Готово: $target"
