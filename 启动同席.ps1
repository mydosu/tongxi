$ErrorActionPreference = 'Stop'
$appExecutable = Join-Path $PSScriptRoot 'release\Agent Hub.exe'
if (-not (Test-Path -LiteralPath $appExecutable -PathType Leaf)) {
    throw '未找到桌面程序，请先执行 npm run desktop:build。'
}
Start-Process -FilePath $appExecutable -WorkingDirectory (Split-Path -Parent $appExecutable) -WindowStyle Hidden
