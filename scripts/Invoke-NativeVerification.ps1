# Only the explicitly authorized temporary MWB pause watcher needs UAC.
# The desktop app and real Codex verification run unelevated.
param([ValidateSet('verify_native_input.py', 'verify_stage3_native.py', 'verify_stage4_native.py', 'verify_stage5_native.py', 'verify_stage6_native.py', 'verify_stage7_native.py', 'verify_stage8_native.py', 'verify_hud_physical.py', 'verify_tray_icon_physical.py')][string]$VerificationScript = 'verify_native_input.py',
      [string]$PythonExecutable = 'python')
$ErrorActionPreference = 'Stop'
$projectDirectory = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $projectDirectory
$controlDirectory = Join-Path $projectDirectory ('artifacts\mwb-pause-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $controlDirectory | Out-Null
$pauseScript = Join-Path $PSScriptRoot 'Watch-MwbPause.ps1'
$powershellExecutable = (Get-Process -Id $PID).Path
$pauseArguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"' + $pauseScript + '"'), '-ControlDirectory', ('"' + $controlDirectory + '"'), '-OwnerPid', $PID)
$pauseProcess = Start-Process -FilePath $powershellExecutable -ArgumentList $pauseArguments -Verb RunAs -WindowStyle Hidden -PassThru
$verificationExit = 1
try {
    $readyDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath (Join-Path $controlDirectory 'ready')) -and [DateTime]::UtcNow -lt $readyDeadline) { Start-Sleep -Milliseconds 100 }
    if (-not (Test-Path -LiteralPath (Join-Path $controlDirectory 'ready'))) { throw '键鼠共享暂停助手未就绪。' }
    Start-Sleep -Milliseconds 500
    & $PythonExecutable (Join-Path $PSScriptRoot $VerificationScript) 'release/Agent Hub.exe'
    $verificationExit = $LASTEXITCODE
} finally {
    'stop' | Set-Content -LiteralPath (Join-Path $controlDirectory 'stop') -Encoding utf8
    $restoreDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath (Join-Path $controlDirectory 'restored.json')) -and [DateTime]::UtcNow -lt $restoreDeadline) { Start-Sleep -Milliseconds 100 }
    $restoreFile = Join-Path $controlDirectory 'restored.json'
    if (-not (Test-Path -LiteralPath $restoreFile)) { throw '键鼠共享恢复记录未生成，请检查暂停助手。' }
    Copy-Item -LiteralPath $restoreFile -Destination (Join-Path $projectDirectory 'artifacts\mwb-restoration.json')
    $restoreResult = Get-Content -LiteralPath $restoreFile -Raw | ConvertFrom-Json
    Write-Output ('键鼠共享辅助进程已恢复：' + $restoreResult.helper_restored)
    if (-not $restoreResult.helper_restored) { throw '键鼠共享辅助进程未恢复。' }
}
exit $verificationExit
