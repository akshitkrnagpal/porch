# Install the latest Porch release for this user on Windows, without admin
# rights or Python:
#
#   irm https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.ps1 | iex
#
# Downloads the Windows archive and checks it against the release's
# SHA256SUMS. That file comes from the same release, so it shows the download
# is intact, not who built it. If the GitHub CLI (gh 2.49 or newer, logged in)
# is available, the script also checks the archive's signed build
# attestation, which shows Porch's release workflow built it from the release
# tag; otherwise it says the check was skipped. Then it installs porch.exe and
# porch-tray.exe into %LOCALAPPDATA%\Programs\Porch\bin and adds that folder
# to your user PATH.
#
# $env:PORCH_VERSION = "0.5.0" installs a specific version.
# $env:PORCH_UPDATE_VERIFY = "require" refuses to install unless the
# attestation verifies (default: auto, check it when gh can).
#
# Everything runs inside one script block: its settings stay out of your
# session, and a download cut short doesn't parse, so nothing runs.
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 downloads far more slowly with a progress bar.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2 by default; GitHub needs it.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'akshitkrnagpal/porch'
    $target = 'x86_64-pc-windows-msvc'

    $verify = "$env:PORCH_UPDATE_VERIFY".Trim().ToLowerInvariant()
    if ($verify -eq '') { $verify = 'auto' }
    if ($verify -ne 'auto' -and $verify -ne 'require') {
        throw "PORCH_UPDATE_VERIFY must be 'require' or 'auto' (the default), got '$env:PORCH_UPDATE_VERIFY'"
    }

    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
        throw "Porch publishes Windows builds for x64 only; build from source: cargo install --locked --git https://github.com/$repo porch"
    }

    if ($env:PORCH_VERSION) {
        $version = $env:PORCH_VERSION.Trim().TrimStart('v')
    } else {
        # Follow the /releases/latest redirect to its tag. Unlike the API,
        # this has no 60-requests-an-hour limit per address.
        $request = [Net.HttpWebRequest]::Create("https://github.com/$repo/releases/latest")
        $request.Method = 'HEAD'
        $request.AllowAutoRedirect = $false
        $request.UserAgent = 'porch-install'
        try {
            $response = $request.GetResponse()
        } catch {
            throw "cannot reach github.com to find the latest release: $($_.Exception.Message)"
        }
        try {
            $location = $response.Headers['Location']
        } finally {
            $response.Close()
        }
        if ($location -notmatch '/releases/tag/v?([^/?#]+)$') {
            throw "cannot find the latest release (github.com/$repo/releases/latest went to '$location')"
        }
        $version = $Matches[1]
    }
    if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$') {
        throw "'$version' is not a release version"
    }

    # Run gh with its output captured; Windows PowerShell 5.1 turns a native
    # program's stderr into errors when it is redirected.
    function Invoke-Gh([string]$Gh, [string[]]$Arguments) {
        $info = New-Object System.Diagnostics.ProcessStartInfo
        $info.FileName = $Gh
        $info.Arguments = ($Arguments | ForEach-Object { '"' + $_ + '"' }) -join ' '
        $info.UseShellExecute = $false
        $info.RedirectStandardOutput = $true
        $info.RedirectStandardError = $true
        $info.CreateNoWindow = $true
        $process = [Diagnostics.Process]::Start($info)
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        [pscustomobject]@{ Code = $process.ExitCode; Out = $stdout.Result; Err = $stderr }
    }

    # Say the attestation could not be checked, or stop when it is required.
    function Skip-Attestation([string]$Reason) {
        if ($verify -eq 'require') {
            throw "PORCH_UPDATE_VERIFY=require: cannot check the build attestation: $Reason"
        }
        Write-Host "Skipped the build attestation check: $Reason."
        Write-Host '  Only the SHA-256 checksum was checked. It comes from the same release, so it shows'
        Write-Host "  the download is intact, not that Porch's release workflow built it. To check that,"
        Write-Host "  install the GitHub CLI (2.49 or newer) and run 'gh auth login' (or set GH_TOKEN),"
        Write-Host '  then rerun this script. PORCH_UPDATE_VERIFY=require refuses to install without it.'
    }

    function Test-Attestation([string]$Archive) {
        $gh = Get-Command gh -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $gh) {
            Skip-Attestation 'the GitHub CLI (gh) is not installed'
            return
        }
        try {
            $help = Invoke-Gh $gh.Source @('attestation', 'verify', '--help')
        } catch {
            Skip-Attestation "gh could not run: $($_.Exception.Message)"
            return
        }
        if ($help.Code -ne 0) {
            Skip-Attestation "this gh has no 'attestation' command (it needs gh 2.49 or newer)"
            return
        }
        $workflow = "$repo/.github/workflows/release.yml"
        $arguments = @('attestation', 'verify', $Archive, '--repo', $repo, '--signer-workflow', $workflow)
        $pinned = $help.Out -match '--source-ref'
        if ($pinned) {
            $arguments += @('--source-ref', "refs/tags/v$version")
        } elseif ($verify -eq 'require') {
            throw "PORCH_UPDATE_VERIFY=require: this gh can't check which tag the release was built from (no --source-ref); update the GitHub CLI"
        }
        $result = Invoke-Gh $gh.Source $arguments
        if ($result.Code -eq 0) {
            if ($pinned) {
                Write-Host "Verified build attestation: built by $workflow from refs/tags/v$version"
            } else {
                Write-Host "Verified build attestation: built by $workflow (update gh to also check the release tag)"
            }
            return
        }
        foreach ($line in ($result.Err.Trim() -split "`r?`n")) { Write-Host "  gh: $line" }
        if ($result.Code -eq 4) {
            Skip-Attestation "gh is not logged in (run 'gh auth login', or set GH_TOKEN)"
        } elseif ((Invoke-Gh $gh.Source @('api', '--silent', 'rate_limit')).Code -eq 0) {
            # GitHub answered, so the archive really has no matching attestation.
            throw 'the build attestation did not verify; not installing'
        } else {
            Skip-Attestation 'gh could not reach the GitHub API, or its token was rejected'
        }
    }

    # Replace $Destination with $Source. A running executable can't be
    # overwritten or deleted, only renamed, so the old one is moved aside to
    # NAME.<pid>.old and deleted once nothing runs it.
    function Install-Executable([string]$Source, [string]$Destination) {
        $directory = Split-Path -Parent $Destination
        $name = Split-Path -Leaf $Destination
        Get-ChildItem -LiteralPath $directory -File |
            Where-Object { $_.Name -match ('^' + [regex]::Escape($name) + '(\.[0-9]+)?\.old$') } |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
        $staged = Join-Path $directory ".$name.$PID.new"
        Copy-Item -LiteralPath $Source -Destination $staged -Force
        $aside = $null
        try {
            if (Test-Path -LiteralPath $Destination) {
                $aside = "$Destination.$PID.old"
                Move-Item -LiteralPath $Destination -Destination $aside -Force
            }
            Move-Item -LiteralPath $staged -Destination $Destination
        } catch {
            $failure = $_.Exception.Message
            if ($aside -and -not (Test-Path -LiteralPath $Destination)) {
                Move-Item -LiteralPath $aside -Destination $Destination -ErrorAction SilentlyContinue
            }
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            throw "cannot replace ${Destination}: $failure"
        }
        if ($aside) { Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue }
        Write-Host "Installed $Destination"
    }

    $name = "porch-$version-$target"
    $base = "https://github.com/$repo/releases/download/v$version"
    $work = Join-Path ([IO.Path]::GetTempPath()) ("porch-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $archive = Join-Path $work "$name.tar.gz"
        Write-Host "Downloading Porch $version for $target"
        Invoke-WebRequest "$base/$name.tar.gz" -OutFile $archive -UseBasicParsing
        Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $work 'SHA256SUMS') -UseBasicParsing
        $pattern = '^([0-9a-fA-F]{64})\s+\*?' + [regex]::Escape("$name.tar.gz") + '$'
        $expected = Get-Content (Join-Path $work 'SHA256SUMS') |
            Where-Object { $_ -match $pattern } |
            ForEach-Object { $Matches[1].ToLowerInvariant() } |
            Select-Object -First 1
        if (-not $expected) { throw "SHA256SUMS has no entry for $name.tar.gz" }
        $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { throw "checksum mismatch for $name.tar.gz" }
        Write-Host "Verified SHA-256 $actual"

        Test-Attestation $archive

        # Windows' own bsdtar. GNU tar from Git for Windows, if it comes first
        # on PATH, reads C:\... as a remote host and fails.
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        if (-not (Test-Path -LiteralPath $tar)) {
            throw "$tar is missing (it ships with Windows 10 1803 and later)"
        }
        & $tar -xzf $archive -C $work
        if ($LASTEXITCODE -ne 0) { throw "tar could not extract $name.tar.gz (exit code $LASTEXITCODE)" }
        if (-not (Test-Path -LiteralPath (Join-Path $work "$name\porch.exe"))) {
            throw "$name.tar.gz does not contain porch.exe"
        }

        $bin = Join-Path $env:LOCALAPPDATA 'Programs\Porch\bin'
        New-Item -ItemType Directory -Force -Path $bin | Out-Null
        foreach ($exe in 'porch.exe', 'porch-tray.exe') {
            $source = Join-Path $work "$name\$exe"
            if (Test-Path -LiteralPath $source) {
                Install-Executable $source (Join-Path $bin $exe)
            }
        }
        $porch = Join-Path $bin 'porch.exe'
        $reported = (& $porch --version | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $reported -ne "porch $version") {
            throw "the installed $porch reports '$reported' instead of 'porch $version'"
        }

        # Edit the user Path as stored, so entries like %USERPROFILE%\bin stay
        # unexpanded, and keep it a REG_EXPAND_SZ value.
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $entries = @($raw -split ';' | Where-Object { $_ -ne '' })
            $present = $entries | Where-Object {
                [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -eq $bin.TrimEnd('\')
            }
            if (-not $present) {
                $key.SetValue('Path', (($entries + $bin) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
                # Setting any user variable through .NET tells Explorer, and so
                # terminals opened from now on, that the environment changed.
                [Environment]::SetEnvironmentVariable('PORCH_INSTALL_REFRESH', '1', 'User')
                [Environment]::SetEnvironmentVariable('PORCH_INSTALL_REFRESH', [NullString]::Value, 'User')
                $env:Path = "$env:Path;$bin"
                Write-Host "Added $bin to your PATH."
            }
        } finally {
            $key.Close()
        }
        Write-Host 'Next: run `porch setup`, or start an app with `porch run <your dev command>`.'
    } finally {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
}
