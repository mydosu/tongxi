$ErrorActionPreference = 'Stop'
$appExecutable = Join-Path $PSScriptRoot 'release\Agent Hub.exe'
if (-not (Test-Path -LiteralPath $appExecutable -PathType Leaf)) {
    throw 'Desktop app not found. Run npm run desktop:build first.'
}
Start-Process -FilePath $appExecutable -WorkingDirectory (Split-Path -Parent $appExecutable) -WindowStyle Hidden
