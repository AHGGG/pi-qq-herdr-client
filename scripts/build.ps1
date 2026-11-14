param([int]$Jobs = 4)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    cargo build --locked --release -p pi-qq-herdr-client -j $Jobs
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $packageRoot = Join-Path $projectRoot 'dist/pi-qq-herdr-client-windows-x64'
    New-Item -ItemType Directory -Force $packageRoot | Out-Null
    Copy-Item 'target/release/pi-qq-herdr-client.exe' $packageRoot -Force
    Copy-Item 'README.md' $packageRoot -Force
    $archivePath = Join-Path $projectRoot 'dist/pi-qq-herdr-client-windows-x64.zip'
    Compress-Archive -Path (Join-Path $packageRoot '*') -DestinationPath $archivePath -Force
    $digest = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$archivePath.sha256", "$digest  pi-qq-herdr-client-windows-x64.zip`n", [Text.UTF8Encoding]::new($false))
    Write-Output $archivePath
    Write-Output "SHA256: $digest"
} finally {
    Pop-Location
}
