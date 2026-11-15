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
    $distRoot = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Force $distRoot | Out-Null
    $exeName = 'pi-qq-herdr-client-windows-x64.exe'
    $exePath = Join-Path $distRoot $exeName
    Copy-Item 'target/release/pi-qq-herdr-client.exe' $exePath -Force
    $hasher = [Security.Cryptography.SHA256]::Create()
    $exeStream = [IO.File]::OpenRead($exePath)
    try {
        $digest = ([BitConverter]::ToString($hasher.ComputeHash($exeStream))).Replace('-', '').ToLowerInvariant()
    } finally {
        $exeStream.Dispose()
        $hasher.Dispose()
    }
    [IO.File]::WriteAllText("$exePath.sha256", "$digest  $exeName`n", [Text.UTF8Encoding]::new($false))
    Write-Output $exePath
    Write-Output "SHA256: $digest"
} finally {
    Pop-Location
}
