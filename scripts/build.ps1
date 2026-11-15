param([int]$Jobs = 4)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    # Windows PowerShell may turn Cargo progress on stderr into terminating
    # NativeCommandError records when output is captured. Use Cargo's exit code.
    $nativePreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        cargo build --locked --release -p pi-qq-herdr-client -j $Jobs
        $cargoExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $nativePreference
    }
    if ($cargoExitCode -ne 0) { throw "Release build failed ($cargoExitCode)." }
    $packageRoot = Join-Path $projectRoot 'dist/pi-qq-herdr-client-windows-x64'
    New-Item -ItemType Directory -Force $packageRoot | Out-Null
    Copy-Item 'target/release/pi-qq-herdr-client.exe' $packageRoot -Force
    Copy-Item 'README.md' $packageRoot -Force
    $archivePath = Join-Path $projectRoot 'dist/pi-qq-herdr-client-windows-x64.zip'
    Compress-Archive -Path (Join-Path $packageRoot '*') -DestinationPath $archivePath -Force
    $hasher = [Security.Cryptography.SHA256]::Create()
    $archiveStream = [IO.File]::OpenRead($archivePath)
    try {
        $digest = ([BitConverter]::ToString($hasher.ComputeHash($archiveStream))).Replace('-', '').ToLowerInvariant()
    } finally {
        $archiveStream.Dispose()
        $hasher.Dispose()
    }
    [IO.File]::WriteAllText("$archivePath.sha256", "$digest  pi-qq-herdr-client-windows-x64.zip`n", [Text.UTF8Encoding]::new($false))
    Write-Output $archivePath
    Write-Output "SHA256: $digest"
} finally {
    Pop-Location
}
