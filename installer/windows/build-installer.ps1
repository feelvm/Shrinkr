#Requires -Version 5.1
<#
.SYNOPSIS
    Builds the Shrinkr Windows installer (Shrinkr-Setup-<version>-x64.exe).

.DESCRIPTION
    Compiles the GUI and CLI in release mode, stages them next to the
    bundled FFmpeg sidecars and FFMPEG-NOTES.txt, and runs the Inno Setup
    compiler on installer/windows/shrinkr.iss.

    FFmpeg is downloaded from the same BtbN build the release workflow
    uses and cached under target/installer/ffmpeg, so repeated runs are
    offline. Pass -FfmpegDir to reuse a directory that already contains
    ffmpeg.exe and ffprobe.exe (the release workflow passes its sidecars
    folder).

    Requires Inno Setup 6+:  winget install JRSoftware.InnoSetup

.PARAMETER Version
    Installer version. Defaults to the `shrinkr` package version in
    Cargo.toml. The release workflow passes the tag without the leading
    "v".

.PARAMETER TargetTriple
    Cargo target triple to take the binaries from (for example
    x86_64-pc-windows-msvc). Omit to use the host's target/release folder.

.PARAMETER FfmpegDir
    Directory containing ffmpeg.exe and ffprobe.exe (searched
    recursively). Omit to download/cache the bundled FFmpeg build.

.PARAMETER OutputDir
    Where the installer is written. Defaults to <repo>/dist.

.PARAMETER FfmpegUrl
    Override the FFmpeg archive URL (defaults to the BtbN GPL build used
    by .github/workflows/release.yml).

.PARAMETER SkipBuild
    Do not run cargo build; reuse existing binaries.

.EXAMPLE
    ./installer/windows/build-installer.ps1

.EXAMPLE
    ./installer/windows/build-installer.ps1 -SkipBuild -TargetTriple x86_64-pc-windows-msvc -FfmpegDir sidecars -OutputDir .
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$TargetTriple,
    [string]$FfmpegDir,
    [string]$OutputDir,
    [string]$FfmpegUrl = 'https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ProgressPreference = 'SilentlyContinue'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

function Get-ShrinkrCargoVersion {
    $manifest = Join-Path $RepoRoot 'Cargo.toml'
    $metadata = & cargo metadata --no-deps --format-version 1 --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo metadata failed'
    }
    $package = $metadata | ConvertFrom-Json |
        Select-Object -ExpandProperty packages |
        Where-Object { $_.name -eq 'shrinkr' } |
        Select-Object -First 1
    if (-not $package) {
        throw 'could not find the `shrinkr` package in cargo metadata'
    }
    return $package.version
}

function Get-NumericVersion([string]$Value) {
    $core = ($Value -split '[-+]')[0]
    $parts = @($core.Split('.'))
    if ($parts.Count -gt 4) {
        throw "version '$Value' has more than four numeric components"
    }
    while ($parts.Count -lt 4) {
        $parts += '0'
    }
    $numbers = foreach ($part in $parts) {
        if ($part -notmatch '^\d+$' -or [int]$part -gt 65535) {
            throw "version '$Value' has a non-numeric or out-of-range component '$part'"
        }
        [int]$part
    }
    return ($numbers -join '.')
}

function Find-FfmpegExes([string]$Dir) {
    if (-not $Dir -or -not (Test-Path -LiteralPath $Dir)) {
        return $null
    }
    $ffmpeg = Get-ChildItem -LiteralPath $Dir -Recurse -Filter 'ffmpeg.exe' -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $ffprobe = Get-ChildItem -LiteralPath $Dir -Recurse -Filter 'ffprobe.exe' -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $ffmpeg -or -not $ffprobe) {
        return $null
    }
    return [pscustomobject]@{
        Ffmpeg  = $ffmpeg.FullName
        Ffprobe = $ffprobe.FullName
    }
}

function Find-Iscc {
    $programFilesX86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    $candidates = @(
        $env:ISCC,
        (Get-Command iscc.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 7\ISCC.exe'),
        $(if ($programFilesX86) { Join-Path $programFilesX86 'Inno Setup 6\ISCC.exe' }),
        $(if ($programFilesX86) { Join-Path $programFilesX86 'Inno Setup 7\ISCC.exe' }),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 7\ISCC.exe')
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
    throw 'Inno Setup compiler (ISCC.exe) not found. Install it with: winget install JRSoftware.InnoSetup'
}

if (-not $Version) {
    $Version = Get-ShrinkrCargoVersion
}
$numericVersion = Get-NumericVersion $Version
# Keep the output filename safe even if the version has build metadata.
$fileVersion = $Version -replace '[^0-9A-Za-z._-]', '-'

if (-not $OutputDir) {
    $OutputDir = Join-Path $RepoRoot 'dist'
} elseif (-not [System.IO.Path]::IsPathRooted($OutputDir)) {
    $OutputDir = Join-Path (Get-Location).Path $OutputDir
}
$OutputDir = [System.IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null

if ($TargetTriple) {
    $binDir = Join-Path $RepoRoot "target\$TargetTriple\release"
} else {
    $binDir = Join-Path $RepoRoot 'target\release'
}

if (-not $SkipBuild) {
    Write-Host "==> Building Shrinkr $Version (release)"
    $cargoArgs = @('build', '--release', '-p', 'shrinkr', '-p', 'shrinkr-cli')
    if ($TargetTriple) {
        $cargoArgs += @('--target', $TargetTriple)
    }
    Push-Location $RepoRoot
    try {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

foreach ($binary in 'shrinkr.exe', 'shrinkr-cli.exe') {
    $path = Join-Path $binDir $binary
    if (-not (Test-Path -LiteralPath $path)) {
        throw "missing $path - run without -SkipBuild, or pass -TargetTriple for a cross-target build"
    }
}

if ($FfmpegDir) {
    $FfmpegDir = [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path $FfmpegDir))
    $ffmpeg = Find-FfmpegExes $FfmpegDir
    if (-not $ffmpeg) {
        throw "no ffmpeg.exe and ffprobe.exe found under $FfmpegDir"
    }
} else {
    $ffmpegCache = Join-Path $RepoRoot 'target\installer\ffmpeg'
    $ffmpeg = Find-FfmpegExes $ffmpegCache
    if (-not $ffmpeg) {
        Write-Host "==> Downloading FFmpeg sidecars"
        Write-Host "    $FfmpegUrl"
        $downloadDir = Join-Path $RepoRoot 'target\installer'
        New-Item -ItemType Directory -Path $downloadDir -Force | Out-Null
        $archive = Join-Path $downloadDir 'ffmpeg-download.zip'
        Invoke-WebRequest -Uri $FfmpegUrl -OutFile $archive -UseBasicParsing
        New-Item -ItemType Directory -Path $ffmpegCache -Force | Out-Null
        Expand-Archive -LiteralPath $archive -DestinationPath $ffmpegCache -Force
        Remove-Item -LiteralPath $archive -Force
        $ffmpeg = Find-FfmpegExes $ffmpegCache
        if (-not $ffmpeg) {
            throw "the downloaded archive did not contain ffmpeg.exe/ffprobe.exe (searched $ffmpegCache)"
        }
    }
}

$notes = Join-Path $RepoRoot 'FFMPEG-NOTES.txt'
if (-not (Test-Path -LiteralPath $notes)) {
    throw "missing $notes"
}

$staging = Join-Path $RepoRoot 'target\installer\staging'
if (Test-Path -LiteralPath $staging) {
    Remove-Item -LiteralPath $staging -Recurse -Force
}
New-Item -ItemType Directory -Path $staging -Force | Out-Null

Write-Host "==> Staging files"
Copy-Item -LiteralPath (Join-Path $binDir 'shrinkr.exe') -Destination $staging
Copy-Item -LiteralPath (Join-Path $binDir 'shrinkr-cli.exe') -Destination $staging
Copy-Item -LiteralPath $ffmpeg.Ffmpeg -Destination (Join-Path $staging 'ffmpeg.exe')
Copy-Item -LiteralPath $ffmpeg.Ffprobe -Destination (Join-Path $staging 'ffprobe.exe')
Copy-Item -LiteralPath $notes -Destination $staging
Get-ChildItem -LiteralPath $staging | ForEach-Object {
    Write-Host ('    {0,-18} {1,8:N1} MB' -f $_.Name, ($_.Length / 1MB))
}

$iscc = Find-Iscc
$script = Join-Path $PSScriptRoot 'shrinkr.iss'
Write-Host "==> Compiling installer with Inno Setup"
$isccArgs = @(
    "/DAppVersion=$Version",
    "/DNumericVersion=$numericVersion",
    "/DFileVersion=$fileVersion",
    "/DStagingDir=$staging",
    "/DOutputDir=$OutputDir",
    $script
)
& $iscc @isccArgs
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup failed with exit code $LASTEXITCODE"
}

$setup = Join-Path $OutputDir "Shrinkr-Setup-$fileVersion-x64.exe"
if (-not (Test-Path -LiteralPath $setup)) {
    throw "expected installer was not produced: $setup"
}
$sizeMb = [math]::Round((Get-Item -LiteralPath $setup).Length / 1MB, 1)
Write-Host ''
Write-Host "Installer ready: $setup ($sizeMb MB)"

$setup
