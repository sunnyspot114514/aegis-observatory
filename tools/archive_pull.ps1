# Pull a loopback export batch from the stick over USB SSH. Does not prune.
# Required: AEGIS_ARCHIVE_ROOT (D: originals). Optional: AEGIS_SSH_HOST, AEGIS_EXPORT_FROM, AEGIS_EXPORT_LIMIT.
$ErrorActionPreference = "Stop"
$root = $env:AEGIS_ARCHIVE_ROOT
if (-not $root) { throw "set AEGIS_ARCHIVE_ROOT to the originals directory (not GitHub)" }
$hostName = $env:AEGIS_SSH_HOST
if (-not $hostName) { throw "set AEGIS_SSH_HOST (USB SSH), never the public hostname" }
$from = if ($env:AEGIS_EXPORT_FROM) { $env:AEGIS_EXPORT_FROM } else { "1" }
$limit = if ($env:AEGIS_EXPORT_LIMIT) { $env:AEGIS_EXPORT_LIMIT } else { "50" }
$stamp = [DateTime]::UtcNow.ToString("yyyyMMddTHHmmssZ")
$dest = Join-Path $root "batch-$stamp"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
$remoteTmp = "/tmp/aegis-export-$stamp.json"
$remote = "curl -sS --max-time 20 'http://127.0.0.1:8787/ops/export?from=$from&limit=$limit' -o $remoteTmp && sha256sum $remoteTmp"
& ssh.exe -o BatchMode=no -o StrictHostKeyChecking=accept-new $hostName $remote
$batch = Join-Path $dest "batch.json"
& scp.exe -o BatchMode=no -o StrictHostKeyChecking=accept-new "${hostName}:${remoteTmp}" $batch
& ssh.exe -o BatchMode=no -o StrictHostKeyChecking=accept-new $hostName "rm -f $remoteTmp"
python "$PSScriptRoot\archive_verify.py" $dest
Write-Host "PULL_OK $dest"
Write-Host "Prune is not performed."
