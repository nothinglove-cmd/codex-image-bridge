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
$releaseExecutable = Join-Path $dist "CodexImageFix.exe"

if (-not (Test-Path -LiteralPath $releaseExecutable -PathType Leaf)) {
    throw "release executable is missing: $releaseExecutable"
}

Push-Location $projectRoot
try {
    $hash = (Get-FileHash -LiteralPath $releaseExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath (Join-Path $dist "CodexImageFix.exe.sha256") -Encoding ascii -Value "$hash  CodexImageFix.exe"

    $metadata = Invoke-NativeChecked {
        cargo metadata --format-version 1 --locked
    } "cargo metadata" | ConvertFrom-Json
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

    Invoke-NativeChecked { cargo tree --locked } "cargo tree" |
        Set-Content -LiteralPath (Join-Path $dist "THIRD-PARTY-DEPENDENCIES.txt") -Encoding utf8
    $rustcVersion = Invoke-NativeChecked { rustc --version --verbose } "rustc --version" | Out-String
    $cargoVersion = Invoke-NativeChecked { cargo --version } "cargo --version"
    @(
        "builtAtUtc=$((Get-Date).ToUniversalTime().ToString('o'))"
        "rustc=$rustcVersion"
        "cargo=$cargoVersion"
        "target=$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
        "os=$([System.Environment]::OSVersion.VersionString)"
        "sha256=$hash"
    ) | Set-Content -LiteralPath (Join-Path $dist "BUILD-INFO.txt") -Encoding utf8
} finally {
    Pop-Location
}
