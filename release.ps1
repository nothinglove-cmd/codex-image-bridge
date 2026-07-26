param(
    [string]$OutputDirectory = "dist"
)

$ErrorActionPreference = "Stop"

function Invoke-NativeChecked {
    param(
        [Parameter(Mandatory = $true)]
        [scriptblock]$Command,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    $output = & $Command
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "$Name failed with exit code $exitCode"
    }
    $output
}

$projectRoot = $PSScriptRoot
$dist = if ([System.IO.Path]::IsPathRooted($OutputDirectory)) {
    [System.IO.Path]::GetFullPath($OutputDirectory)
} else {
    [System.IO.Path]::GetFullPath((Join-Path $projectRoot $OutputDirectory))
}
$targetDirectory = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    Join-Path $projectRoot "target"
} elseif ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
    [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
} else {
    [System.IO.Path]::GetFullPath((Join-Path $projectRoot $env:CARGO_TARGET_DIR))
}
$executable = Join-Path $targetDirectory "release\CodexImageFix.exe"

Push-Location $projectRoot
try {
    Invoke-NativeChecked { cargo fmt --all -- --check } "cargo fmt"
    Invoke-NativeChecked { cargo clippy --locked --all-targets -- -D warnings } "cargo clippy"
    Invoke-NativeChecked { cargo test --locked } "cargo test"
    Invoke-NativeChecked { cargo build --locked --release } "cargo build"

    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    Copy-Item -LiteralPath $executable -Destination (Join-Path $dist "CodexImageFix.exe") -Force
    & (Join-Path $projectRoot "generate-release-metadata.ps1") -OutputDirectory $dist
} finally {
    Pop-Location
}
