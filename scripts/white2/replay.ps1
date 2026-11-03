param(
    [string]$Inputs = "$PSScriptRoot/baseline.inputs",
    [UInt64]$Frames = 2300,
    [string]$Cache = '.gen5-cache/pokemon-white-2/95AB5437',
    [string]$Rom = 'roms/Pokemon White Version 2.nds',
    [UInt64]$Seconds = 180,
    [string]$Touches,
    [UInt64]$ScreenshotEvery = 120
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')
$run = Join-Path '.gen5-cache/checkpoints' ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $run | Out-Null
$names = 'GEN5_INPUT', 'GEN5_TOUCH', 'GEN5_STATE_DIR', 'GEN5_BOOT_SECONDS', 'GEN5_SCREEN_EVERY'
$previous = @{}
foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name) }
try {
    $entries = Get-Content -LiteralPath $Inputs | ForEach-Object { ($_ -split '#', 2)[0].Trim() } | Where-Object { $_ }
    $env:GEN5_INPUT = $entries -join ','
    $env:GEN5_TOUCH = ''
    if ($Touches) {
        $env:GEN5_TOUCH = (Get-Content -LiteralPath $Touches | ForEach-Object { ($_ -split '#', 2)[0].Trim() } | Where-Object { $_ }) -join ','
    }
    $env:GEN5_STATE_DIR = Join-Path $run 'state'
    $env:GEN5_BOOT_SECONDS = "$Seconds"
    $env:GEN5_SCREEN_EVERY = "$ScreenshotEvery"
    & cargo run --release -- boot $Cache $Rom $Frames (Join-Path $run 'screens') |
        Tee-Object -FilePath (Join-Path $run 'report.txt')
    if ($LASTEXITCODE -ne 0) { throw "Replay failed with exit code $LASTEXITCODE" }
    if (-not (Select-String -LiteralPath (Join-Path $run 'report.txt') -SimpleMatch "=== stopped: reached frame $Frames" -Quiet)) {
        throw "Replay stopped before frame $Frames; inspect $run/report.txt"
    }
    Write-Host "Replay artifacts: $run (frame completion only; inspect screenshots for gameplay success)"
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $previous[$name]) }
}
