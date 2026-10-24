[CmdletBinding()]
param(
    [Alias('factorio-dir')][string]$FactorioDir,
    [Alias('mods-dir')][string]$ModsDir,
    [string]$Version
)

$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or
    ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64')) {
    throw 'No companion release archive is published for this OS/architecture (Windows x86_64 required).'
}
if (-not $FactorioDir) { $FactorioDir = Join-Path $env:APPDATA 'Factorio' }
$FactorioDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($FactorioDir)
if (-not $ModsDir) { $ModsDir = Join-Path $FactorioDir 'mods' }
$ModsDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ModsDir)
$releases = 'https://github.com/vshuraeff/ptt-radio/releases'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
if ($env:PTT_RADIO_INSTALL_TEST_RELEASE_URL) {
    $releases = $env:PTT_RADIO_INSTALL_TEST_RELEASE_URL.TrimEnd('/')
}
if (-not $Version) {
    $request = [Net.HttpWebRequest]::Create("$releases/latest")
    $request.Method = 'HEAD'
    $request.AllowAutoRedirect = $false
    $request.Timeout = 20000
    $response = $request.GetResponse()
    try {
        $location = $response.Headers['Location']
        if ([int]$response.StatusCode -notin @(301, 302, 303, 307, 308) -or -not $location) {
            throw 'Could not resolve the latest release tag.'
        }
        $resolved = [Uri]::new([Uri]"$releases/latest", $location).AbsoluteUri
        $prefix = "$releases/tag/"
        if (-not $resolved.StartsWith($prefix, [StringComparison]::Ordinal)) {
            throw 'Unexpected latest release redirect.'
        }
        $Version = $resolved.Substring($prefix.Length)
    } finally { $response.Close() }
}
if ($Version -notmatch '^v[0-9][A-Za-z0-9._-]*$') { throw 'Version must be a tag such as v0.4.0.' }
$asset = "ptt-radio-companion-$($Version.Substring(1))-windows-x86_64.zip"
$installDir = Join-Path $FactorioDir 'ptt-radio-companion'
$null = New-Item -ItemType Directory -Force -Path $FactorioDir
if (Test-Path -LiteralPath $installDir) {
    $existing = Get-Item -LiteralPath $installDir -Force
    if (-not $existing.PSIsContainer -or ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'The install path must be a directory, not a link.'
    }
}
$stage = Join-Path $FactorioDir ('.ptt-radio-install-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $stage
$backup = $null
try {
    $archive = Join-Path $stage $asset
    $sums = Join-Path $stage 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$Version/$asset" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$Version/SHA256SUMS" -OutFile $sums
    $checksums = @(Get-Content -LiteralPath $sums | Where-Object { $_ -match ('^([a-fA-F0-9]{64})  ' + [Regex]::Escape($asset) + '$') })
    if ($checksums.Count -ne 1) { throw 'Missing or invalid checksum.' }
    $expected = $checksums[0].Substring(0, 64)
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) {
        throw 'Checksum mismatch; existing installation was kept.'
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    $newDir = Join-Path $stage 'new'
    $null = New-Item -ItemType Directory -Path $newDir
    try {
        $wanted = @('ptt-radio-companion.exe', 'LICENSE', 'README.md')
        if ($zip.Entries.Count -ne $wanted.Count) { throw 'Unexpected archive layout.' }
        foreach ($name in $wanted) {
            $entries = @($zip.Entries | Where-Object { $_.FullName -ceq $name })
            if ($entries.Count -ne 1) { throw 'Unexpected archive layout.' }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], (Join-Path $newDir $name))
        }
    } finally { $zip.Dispose() }
    if ((Get-Item -LiteralPath (Join-Path $newDir 'ptt-radio-companion.exe')).Length -eq 0) { throw 'Empty companion binary.' }
    $marker = @{ version = 1; mods_dir = $ModsDir; installed_by = 'ptt-radio installer' } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText((Join-Path $newDir 'install.json'), $marker, (New-Object Text.UTF8Encoding($false)))
    if (Test-Path -LiteralPath $installDir) {
        Move-Item -LiteralPath $installDir -Destination (Join-Path $stage 'previous')
        $backup = Join-Path $stage 'previous'
    }
    Move-Item -LiteralPath $newDir -Destination $installDir
    $backup = $null
} finally {
    if ($backup -and -not (Test-Path -LiteralPath $installDir)) {
        try {
            Move-Item -LiteralPath $backup -Destination $installDir
            $backup = $null
        } catch { Write-Warning "Could not restore installation: $($_.Exception.Message)" }
    }
    if ($backup) {
        Write-Warning "Previous installation kept at $backup"
    } else {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}
$binary = Join-Path $installDir 'ptt-radio-companion.exe'
Write-Output "Installed: $installDir"
Write-Output ("Start: & '" + $binary.Replace("'", "''") + "'")
Write-Output ('Steam launch options: "' + $binary + '" --launch %command%')
