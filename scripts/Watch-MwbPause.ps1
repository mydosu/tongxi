param([Parameter(Mandatory)][string]$ControlDirectory, [Parameter(Mandatory)][int]$OwnerPid)
$ErrorActionPreference = 'Stop'
$artifactRoot = [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $PSScriptRoot) 'artifacts'))
$controlRoot = [IO.Path]::GetFullPath($ControlDirectory)
if (-not $controlRoot.StartsWith($artifactRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw '控制目录必须在项目 artifacts 下。' }
$helperExecutable = 'C:\Program Files (x86)\Microsoft Garage\Mouse without Borders\MouseWithoutBordersHelper.exe'
$pausedCount = 0
$deadline = [DateTime]::UtcNow.AddMinutes(8)
try {
    while ([DateTime]::UtcNow -lt $deadline -and -not (Test-Path -LiteralPath (Join-Path $controlRoot 'stop')) -and (Get-Process -Id $OwnerPid -ErrorAction SilentlyContinue)) {
        foreach ($candidate in (Get-Process -Name MouseWithoutBordersHelper -ErrorAction SilentlyContinue)) {
            if ($candidate.Path -eq $helperExecutable) {
                Stop-Process -Id $candidate.Id -Force -ErrorAction Stop
                $pausedCount += 1
            }
        }
        if (-not (Get-Process -Name MouseWithoutBordersHelper -ErrorAction SilentlyContinue) -and -not (Test-Path -LiteralPath (Join-Path $controlRoot 'ready'))) {
            'ready' | Set-Content -LiteralPath (Join-Path $controlRoot 'ready') -Encoding utf8
        }
        Start-Sleep -Milliseconds 20
    }
} finally {
    if (-not (Get-Process -Name MouseWithoutBordersHelper -ErrorAction SilentlyContinue)) {
        Start-Process -FilePath $helperExecutable -WindowStyle Hidden
    }
    Start-Sleep -Milliseconds 500
    [ordered]@{
        paused_helper_process_count = $pausedCount
        elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        helper_paths = @((Get-Process -Name MouseWithoutBordersHelper -ErrorAction SilentlyContinue) | ForEach-Object { [ordered]@{id=$_.Id; path=$_.Path} })
        helper_restored = @(Get-Process -Name MouseWithoutBordersHelper -ErrorAction SilentlyContinue).Count -gt 0
        main_processes_left_running = @(Get-Process -Name MouseWithoutBorders -ErrorAction SilentlyContinue).Count
        settings_changed = $false
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $controlRoot 'restored.json') -Encoding utf8
}
