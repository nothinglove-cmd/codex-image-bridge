param(
    [string]$OutputDirectory = "dist"
)

$ErrorActionPreference = "Stop"
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
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    cargo build --locked --release

    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    Copy-Item -LiteralPath $executable -Destination (Join-Path $dist "CodexImageFix.exe") -Force

    $releaseExecutable = Join-Path $dist "CodexImageFix.exe"
    $hash = (Get-FileHash -LiteralPath $releaseExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath (Join-Path $dist "CodexImageFix.exe.sha256") -Encoding ascii -Value "$hash  CodexImageFix.exe"

    $metadata = cargo metadata --format-version 1 --locked | ConvertFrom-Json
    $components = @(
        $metadata.packages | Sort-Object name, version | ForEach-Object {
            [ordered]@{
                type = "library"
                name = $_.name
                version = $_.version
                licenses = @(
                    if ($_.license) {
                        [ordered]@{ expression = $_.license }
                    }
                )
                purl = "pkg:cargo/$($_.name)@$($_.version)"
            }
        }
    )
    $sbom = [ordered]@{
        bomFormat = "CycloneDX"
        specVersion = "1.5"
        serialNumber = "urn:uuid:$([guid]::NewGuid())"
        version = 1
        metadata = [ordered]@{
            timestamp = (Get-Date).ToUniversalTime().ToString("o")
            component = [ordered]@{
                type = "application"
                name = "CodexImageFix"
                version = (Get-Item -LiteralPath $releaseExecutable).VersionInfo.ProductVersion
                hashes = @([ordered]@{ alg = "SHA-256"; content = $hash })
            }
        }
        components = $components
    }
    $sbom | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $dist "CodexImageFix.sbom.cdx.json") -Encoding utf8

    cargo tree --locked | Set-Content -LiteralPath (Join-Path $dist "THIRD-PARTY-DEPENDENCIES.txt") -Encoding utf8
    @(
        "builtAtUtc=$((Get-Date).ToUniversalTime().ToString('o'))"
        "rustc=$(rustc --version --verbose | Out-String)"
        "cargo=$(cargo --version)"
        "target=$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
        "os=$([System.Environment]::OSVersion.VersionString)"
        "sha256=$hash"
    ) | Set-Content -LiteralPath (Join-Path $dist "BUILD-INFO.txt") -Encoding utf8
} finally {
    Pop-Location
}
