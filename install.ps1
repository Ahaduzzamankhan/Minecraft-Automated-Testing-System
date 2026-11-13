# MATS one-line installer for Windows.
#
#   irm https://raw.githubusercontent.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System/main/install.ps1 | iex
#
# Environment variables:
#   MATS_VERSION      release tag to install (default: latest), e.g. v1.0.0
#   MATS_INSTALL_DIR  install directory (default: %LOCALAPPDATA%\MATS\bin)
$ErrorActionPreference = "Stop"

$Repo = "Ahaduzzamankhan/Minecraft-Automated-Testing-System"
$Bin = "mats"

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "ARM64" { "aarch64" }
    "AMD64" { "x86_64" }
    default { throw "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$tag = $env:MATS_VERSION
if (-not $tag) {
    $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
        -Headers @{ "User-Agent" = "mats-installer" }
    $tag = $latest.tag_name
}
if (-not $tag) { throw "Could not determine the latest MATS release" }
if (-not $tag.StartsWith("v")) { $tag = "v$tag" }

$asset = "$Bin-$tag-windows-$arch.exe"
$base = "https://github.com/$Repo/releases/download/$tag"
$temp = Join-Path $env:TEMP ("mats-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    Write-Host "Downloading MATS $tag for windows-$arch..."
    $download = Join-Path $temp $asset
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $download -UseBasicParsing

    try {
        $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
        $line = ($sums -split "`n") | Where-Object { $_ -match [regex]::Escape($asset) } | Select-Object -First 1
        if ($line) {
            $expected = ($line -split "\s+")[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 -Path $download).Hash.ToLower()
            if ($actual -ne $expected) { throw "Checksum verification failed for $asset" }
            Write-Host "Checksum verified."
        }
    } catch [System.Net.WebException] {
        Write-Warning "No SHA256SUMS published; continuing without checksum verification"
    }

    $installDir = $env:MATS_INSTALL_DIR
    if (-not $installDir) { $installDir = Join-Path $env:LOCALAPPDATA "MATS\bin" }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $target = Join-Path $installDir "$Bin.exe"
    Move-Item -Force -Path $download -Destination $target
    Write-Host "Installed $target"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($userPath) { $parts = $userPath -split ";" | Where-Object { $_ } }
    if (-not ($parts | Where-Object { $_.TrimEnd("\") -ieq $installDir.TrimEnd("\") })) {
        $newPath = (@($parts) + $installDir) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host "Added $installDir to your user PATH."
    }
    $env:Path = "$installDir;$env:Path"

    Write-Host "Open a new terminal, then run: mats --version"
    & $target --version
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $temp
}
