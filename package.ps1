# Builds RDM's setup exe and what an update needs beside it, in target\release\bundle\nsis:
#   RDM_<version>_x64-setup.exe        the setup
#   RDM_<version>_x64-setup.exe.sig    its signature, made with RDM's update key
#   latest.json                        what an installed RDM reads to find this version
#
#   .\package.ps1                                    as it is released
#   .\package.ps1 -Notes "What is new"               with the text the update window shows
#   .\package.ps1 -Config <file> -Url http://..     a build to try an update with (see README,
#                                                    Making a release)
#
# Needs Tauri's command line (cargo install tauri-cli --version "^2" --locked), cargo-about
# (cargo install cargo-about --locked --features cli), the ffmpeg built by the scripts in
# ffmpeg\, and the update key (%USERPROFILE%\.tauri\rdm-update.key, or
# TAURI_SIGNING_PRIVATE_KEY). The version is the one in Cargo.toml
# ([workspace.package]); extension\manifest.json must carry the same (a test checks it).
param(
    # Shown in the update window of an RDM that finds this version.
    [string]$Notes = '',
    # Where the setup will be fetched from; the default is the release page of its version.
    [string]$Url = '',
    # More configuration files, merged in after tauri.bundle.json.
    [string[]]$Config = @()
)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# The id of the key in a minisign public key or signature, as Tauri writes them: base64 of
# a text whose second line is base64 of two bytes, the key's eight-byte id, and the rest.
function KeyId([string]$encoded) {
    $text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($encoded.Trim()))
    $bytes = [Convert]::FromBase64String(($text -split "`n")[1].Trim())
    [BitConverter]::ToString($bytes[2..9])
}

$ffmpeg = Join-Path $root 'target\ffmpeg-min\work\ffmpeg-win64\ffmpeg.exe'
if (-not (Test-Path $ffmpeg)) {
    throw "no ${ffmpeg}: build it first (ffmpeg\README.md)"
}
# Named from where the script was called, before it moves into the repository.
$Config = @($Config | ForEach-Object { (Resolve-Path $_).Path })
$keyBefore = $env:TAURI_SIGNING_PRIVATE_KEY
if (-not $keyBefore) {
    $key = Join-Path $env:USERPROFILE '.tauri\rdm-update.key'
    if (-not (Test-Path $key)) {
        throw "no update key at ${key}: without the key the installed RDMs were given, they take no update"
    }
    $env:TAURI_SIGNING_PRIVATE_KEY = $key
}

Push-Location $root
try {
    # The app's bundle takes rdm.exe and rdm-host.exe as files, so they come first.
    cargo build --release -p rdm
    if ($LASTEXITCODE) { throw 'building rdm and rdm-host failed' }

    # The licences of the libraries in the programs, as one page (about.toml, about.hbs).
    New-Item -ItemType Directory -Force (Join-Path $root 'target\licenses') | Out-Null
    cargo about generate --workspace about.hbs -o target\licenses\libraries.html
    if ($LASTEXITCODE) { throw 'listing the libraries'' licences failed (a licence not in about.toml?)' }

    $out = Join-Path $root 'target\release\bundle\nsis'
    if (Test-Path $out) { Remove-Item (Join-Path $out '*') -Force }

    Push-Location app
    try {
        # tauri.conf.json alone builds without the files above (tests, work on the code);
        # tauri.bundle.json adds what only the setup needs.
        $merge = @('tauri.bundle.json') + $Config | ForEach-Object { '--config'; $_ }
        cargo tauri build --ci @merge
        if ($LASTEXITCODE) { throw 'building the setup failed' }
    }
    finally { Pop-Location }

    $setup = Get-ChildItem $out -Filter '*-setup.exe' | Select-Object -First 1
    if (-not $setup -or -not (Test-Path "$($setup.FullName).sig")) { throw "no signed setup in $out" }
    $signature = (Get-Content "$($setup.FullName).sig" -Raw).Trim()
    # Signed with another key than the one the installed RDMs carry, the build only warns,
    # and every one of them would refuse this setup.
    $public = (Get-Content (Join-Path $root 'app\tauri.conf.json') -Raw | ConvertFrom-Json).plugins.updater.pubkey
    if ((KeyId $signature) -ne (KeyId $public)) {
        throw "the setup was signed with key $(KeyId $signature), but RDM checks updates against key $(KeyId $public) (app\tauri.conf.json)"
    }

    $version = ($setup.Name -split '_')[1]
    if (-not $Url) { $Url = "https://github.com/KatayR/rdm/releases/download/v$version/$($setup.Name)" }
    $latest = [ordered]@{
        version   = $version
        notes     = $Notes
        pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture)
        platforms = @{ 'windows-x86_64' = [ordered]@{ signature = $signature; url = $Url } }
    }
    # Without a byte-order mark: the updater reads it as plain JSON.
    [IO.File]::WriteAllText((Join-Path $out 'latest.json'), ($latest | ConvertTo-Json -Depth 4))

    Get-ChildItem $out | Sort-Object Name | ForEach-Object { '{0,12:N0}  {1}' -f $_.Length, $_.FullName }
}
finally {
    Pop-Location
    $env:TAURI_SIGNING_PRIVATE_KEY = $keyBefore
}
