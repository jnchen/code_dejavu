param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^https://')]
    [string]$DownloadUrl,

    [string]$Version = "",
    [string]$SignaturePath = "",
    [string]$Notes = "",
    [string]$OutputPath = "latest.json"
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

if (-not $Version) {
    $package = Get-Content -Raw -LiteralPath (Join-Path $repoRoot "package.json") | ConvertFrom-Json
    $Version = [string]$package.version
}

if (-not $SignaturePath) {
    $SignaturePath = Join-Path $repoRoot "release_packages\Code Déjà Vu_${Version}_x64-setup.exe.sig"
}
if (-not [System.IO.Path]::IsPathRooted($SignaturePath)) {
    $SignaturePath = Join-Path $repoRoot $SignaturePath
}
$SignaturePath = (Resolve-Path -LiteralPath $SignaturePath).Path

if (-not [System.IO.Path]::IsPathRooted($OutputPath)) {
    $OutputPath = Join-Path $repoRoot $OutputPath
}

$manifest = [ordered]@{
    version = $Version
    notes = $Notes
    pub_date = [DateTimeOffset]::UtcNow.ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
        "windows-x86_64" = [ordered]@{
            signature = (Get-Content -Raw -LiteralPath $SignaturePath).Trim()
            url = $DownloadUrl
        }
    }
}

$json = $manifest | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText($OutputPath, $json + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
Write-Host "Updater manifest written to $OutputPath"
