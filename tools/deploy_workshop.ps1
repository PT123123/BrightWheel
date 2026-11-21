# deploy_workshop.ps1 - bump patch version in Cargo.toml, build release, deploy to C:\workshop\BrightWheel-<ver>
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\deploy_workshop.ps1 [-Root <project root>]
#
# NOTE: keep this file ASCII-only on purpose. On a Chinese-locale system,
# Windows PowerShell 5.1 reads BOM-less UTF-8 as ANSI (GBK) and any non-ASCII
# byte can break script parsing.
param(
    [string]$Root
)
$ErrorActionPreference = 'Stop'

if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }

$cargoToml = Join-Path $Root 'Cargo.toml'
if (!(Test-Path -LiteralPath $cargoToml)) { throw "Cargo.toml not found: $cargoToml" }

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$c = [IO.File]::ReadAllText($cargoToml, [Text.Encoding]::UTF8)

$versionPattern = '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"'
if ($c -notmatch $versionPattern) { throw 'version = "x.y.z" not found in Cargo.toml' }
$major = [int]$Matches[1]
$minor = [int]$Matches[2]
$patch = [int]$Matches[3] + 1
$newVersion = "$major.$minor.$patch"

$c = [regex]::Replace($c, $versionPattern, "version = `"$newVersion`"", 1)
[IO.File]::WriteAllText($cargoToml, $c, $utf8NoBom)
Write-Output "[deploy] bumped version to $newVersion"

Push-Location $Root
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }
} finally { Pop-Location }

$exe = Join-Path $Root 'target\release\BrightWheel.exe'
if (!(Test-Path -LiteralPath $exe)) { throw "Build output missing: $exe" }

$workshop = 'C:\workshop'
$dest = Join-Path $workshop "BrightWheel-$newVersion"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $dest 'BrightWheel.exe') -Force

Write-Host "[ok] deployed to $dest"
Write-Host "Executable: $(Join-Path $dest 'BrightWheel.exe')"
Write-Host "Version: $newVersion"
exit 0
