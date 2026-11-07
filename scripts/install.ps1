# Install the latest Porch release for this user on Windows, without admin
# rights or Python:
#
#   irm https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.ps1 | iex
#
# Downloads the Windows archive, checks it against the release's SHA256SUMS
# and, when the GitHub CLI is installed, its signed build attestation, then
# installs porch.exe and porch-tray.exe into %LOCALAPPDATA%\Programs\Porch\bin
# and adds that folder to your user PATH.
#
# $env:PORCH_VERSION = "0.5.0" installs a specific version.
$ErrorActionPreference = 'Stop'
$repo = 'akshitkrnagpal/porch'
$target = 'x86_64-pc-windows-msvc'
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
    throw "Porch publishes Windows builds for x64 only; build from source: cargo install --git https://github.com/$repo porch"
}
if ($env:PORCH_VERSION) {
    $version = $env:PORCH_VERSION.TrimStart('v')
} else {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'porch-install' }
    $version = $release.tag_name.TrimStart('v')
}
$name = "porch-$version-$target"
$base = "https://github.com/$repo/releases/download/v$version"
$work = Join-Path ([IO.Path]::GetTempPath()) ("porch-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Write-Host "Downloading Porch $version for $target"
    Invoke-WebRequest "$base/$name.tar.gz" -OutFile "$work\$name.tar.gz" -UseBasicParsing
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$work\SHA256SUMS" -UseBasicParsing
    $expected = (Get-Content "$work\SHA256SUMS" | Where-Object { $_ -match "^([0-9a-f]{64})\s+\*?$([regex]::Escape("$name.tar.gz"))$" } | ForEach-Object { $Matches[1] }) | Select-Object -First 1
    if (-not $expected) { throw "SHA256SUMS has no entry for $name.tar.gz" }
    $actual = (Get-FileHash "$work\$name.tar.gz" -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { throw "checksum mismatch for $name.tar.gz" }
    Write-Host "Verified SHA-256 $actual"
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        gh attestation verify "$work\$name.tar.gz" --repo $repo --signer-workflow "$repo/.github/workflows/release.yml" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'the build attestation did not verify; not installing' }
        Write-Host 'Verified build attestation'
    } elseif ($env:PORCH_UPDATE_VERIFY -eq 'require') {
        throw 'PORCH_UPDATE_VERIFY=require needs the GitHub CLI (gh)'
    }
    tar -xzf "$work\$name.tar.gz" -C $work
    $bin = Join-Path $env:LOCALAPPDATA 'Programs\Porch\bin'
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    foreach ($exe in 'porch.exe', 'porch-tray.exe') {
        $source = Join-Path "$work\$name" $exe
        if (Test-Path $source) {
            $destination = Join-Path $bin $exe
            # A running executable can be renamed but not overwritten.
            if (Test-Path $destination) { Move-Item -Force $destination "$destination.old" -ErrorAction SilentlyContinue }
            Copy-Item $source $destination
            Write-Host "Installed $destination"
        }
    }
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($path -split ';') -contains $bin)) {
        [Environment]::SetEnvironmentVariable('Path', ($path.TrimEnd(';') + ";$bin").TrimStart(';'), 'User')
        Write-Host "Added $bin to your PATH; open a new terminal to use porch."
    }
    Write-Host 'Next: run `porch setup`, or start an app with `porch run <your dev command>`.'
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
