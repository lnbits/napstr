#Requires -Version 5.1
<#
.SYNOPSIS
    Builds, signs and verifies an installable Napstrfy APK.

.DESCRIPTION
    Four things have to line up on this machine, and three of them fail in ways
    that do not look like their cause:

      * The Tauri CLI needs JAVA_HOME, ANDROID_HOME and NDK_HOME. Without
        NDK_HOME it tries to install one, cannot do that non-interactively (it
        looks for sdkmanager under cmdline-tools\bin, not cmdline-tools\latest),
        and fails somewhere unrelated.
      * A release APK comes out genuinely unsigned - no META-INF/*.RSA - so it
        cannot be installed as the build leaves it.
      * apksigner MUST run under JDK 17. Whatever `java` resolves to on PATH is
        often an older runtime, which cannot read the PKCS12 debug keystore and
        reports "Invalid keystore format" or fails in PBES2Parameters. Both are
        the same pre-17 limitation and neither means the keystore is damaged.
      * Windows Developer Mode must be on, or the build dies at the jniLibs step
        with "Creation symbolic link is not allowed for this system".

    Signing uses the Android debug key, which is the key the installed app was
    signed with. That is what lets a new APK upgrade it in place. A different key
    cannot upgrade an installed app - it means uninstalling and pairing again.

.PARAMETER Debug
    Build the debug variant instead of release. Keeps symbols, so it is much
    larger (~91 MB against ~36 MB).

.PARAMETER NoSign
    Stop once the build finishes, leaving the unsigned APK where it landed.

.PARAMETER Install
    Install the finished APK over adb. Fails loudly when no device is attached.

.PARAMETER PreflightOnly
    Check the toolchain and free space, then stop. No build, no signing.

.PARAMETER ResetGradle
    Stop the Gradle daemon and drop its transform cache before building.

    This is the remedy for a build that fails during configuration with
    "Could not read workspace metadata from ...transforms\<hash>\metadata.bin",
    naming a dozen different hashes, in a few seconds. The hashes are derived
    from the transform inputs, so they are stable across runs and the same list
    comes back every time - which makes it look like leftover corruption when the
    real cause is a daemon that is still alive.

    The order below is the whole trick. A daemon that outlives the cache being
    cleared keeps serving its own references to the deleted directories, so
    clearing the cache first achieves nothing: stop the daemon, then clear.

.EXAMPLE
    pwsh -File scripts/build-android-apk.ps1
    Builds the release APK, aligns and signs it, verifies the signature, and
    writes the installable to the Desktop.

.EXAMPLE
    pwsh -File scripts/build-android-apk.ps1 -PreflightOnly
    Verifies everything the build needs without starting one.
#>
# Deliberately not [CmdletBinding()]: that would add PowerShell's own common
# -Debug switch, which collides with the one below. Here -Debug means the debug
# APK, and nothing in this script wants the common parameters.
param(
    [switch]$Debug,
    [switch]$NoSign,
    [switch]$Install,
    [switch]$PreflightOnly,
    [switch]$ResetGradle
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Absolute paths throughout, derived from this script's own location. A mangled
# lead-in in a one-liner once turned every path into C:\android\..., so nothing
# here depends on the current working directory.
$root       = Split-Path -Parent $PSScriptRoot
$android    = Join-Path $root 'android'
$project    = Join-Path $android 'src-tauri\gen\android'
$outputs    = Join-Path $project 'app\build\outputs\apk'
$configFile = Join-Path $android 'src-tauri\tauri.conf.json'

# The toolchain is discovered rather than pinned to one machine's layout. Node
# can be machine-wide or per-user, JDK 17 can come from any vendor, and the SDK
# keeps its NDK and build-tools in versioned directories, so a hardcoded path
# only works on the machine it was written for. Every value below can be pinned
# with an environment variable when discovery picks the wrong one.
function Get-FirstExistingPath {
    param([string[]] $Candidates)
    foreach ($candidate in $Candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) { return $candidate }
    }
    return $null
}

function Get-NewestMatchingDirectory {
    param([string] $Root, [string] $Pattern)
    if (-not $Root -or -not (Test-Path -LiteralPath $Root)) { return $null }
    $match = Get-ChildItem -LiteralPath $Root -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like $Pattern } |
        Sort-Object Name -Descending |
        Select-Object -First 1
    if ($match) { return $match.FullName }
    return $null
}

# Join-Path throws on a null base, and discovery is allowed to come up empty so
# the preflight below can report it, so every path is built through this.
function Join-PathOrNull {
    param([string] $Base, [string] $Child)
    if (-not $Base) { return $null }
    return (Join-Path $Base $Child)
}

# Whatever `node` PATH resolves to is the runtime the developer already builds
# with, so it wins over guessing between the machine-wide and per-user installs.
$nodeFromPath = $null
$nodeCommand = Get-Command node.exe -ErrorAction SilentlyContinue
if ($nodeCommand) { $nodeFromPath = Split-Path -Parent $nodeCommand.Source }
$node = Get-FirstExistingPath @(
    $env:NAPSTR_NODE_DIR,
    $nodeFromPath,
    (Get-NewestMatchingDirectory (Join-PathOrNull $env:LOCALAPPDATA 'Programs\nodejs') 'node-v2*'),
    (Join-PathOrNull $env:ProgramFiles 'nodejs'),
    (Join-PathOrNull $env:LOCALAPPDATA 'Programs\nodejs')
)

# JAVA_HOME is only trusted when it really is a 17, because both it and the
# `java` on PATH routinely point at the older runtime apksigner cannot use.
$jdkFromJavaHome = $null
if ($env:JAVA_HOME) {
    $releaseFile = Join-Path $env:JAVA_HOME 'release'
    if ((Test-Path -LiteralPath $releaseFile) -and
        (Select-String -Path $releaseFile -Pattern '^JAVA_VERSION="17' -Quiet)) {
        $jdkFromJavaHome = $env:JAVA_HOME
    }
}
$jdk = Get-FirstExistingPath @(
    $env:NAPSTR_JDK17,
    $jdkFromJavaHome,
    (Get-NewestMatchingDirectory (Join-PathOrNull $env:ProgramFiles 'Microsoft') 'jdk-17*'),
    (Get-NewestMatchingDirectory (Join-PathOrNull $env:ProgramFiles 'Eclipse Adoptium') 'jdk-17*'),
    (Get-NewestMatchingDirectory (Join-PathOrNull $env:ProgramFiles 'Java') 'jdk-17*'),
    (Get-NewestMatchingDirectory (Join-PathOrNull $env:LOCALAPPDATA 'Programs') 'jdk-17*')
)

$sdk = Get-FirstExistingPath @(
    $env:NAPSTR_ANDROID_SDK,
    $env:ANDROID_HOME,
    $env:ANDROID_SDK_ROOT,
    (Join-PathOrNull $env:LOCALAPPDATA 'Android\Sdk')
)

# The project builds against NDK 29 and build-tools 36, so those are preferred
# when they are installed and the newest available version is the fallback.
$ndk = Get-FirstExistingPath @(
    $env:NDK_HOME,
    $env:ANDROID_NDK_HOME,
    (Join-PathOrNull $sdk 'ndk\29.0.13846066'),
    (Get-NewestMatchingDirectory (Join-PathOrNull $sdk 'ndk') '*')
)

$buildTools = Get-FirstExistingPath @(
    $env:NAPSTR_BUILD_TOOLS_DIR,
    (Join-PathOrNull $sdk 'build-tools\36.0.0'),
    (Get-NewestMatchingDirectory (Join-PathOrNull $sdk 'build-tools') '3*')
)
$zipalign   = Join-PathOrNull $buildTools 'zipalign.exe'
$apksigner  = Join-PathOrNull $buildTools 'lib\apksigner.jar'
$keystore   = Join-Path $env:USERPROFILE '.android\debug.keystore'
$adb        = Join-PathOrNull $sdk 'platform-tools\adb.exe'

function Write-Step($message) {
    Write-Host ''
    Write-Host "== $message" -ForegroundColor Cyan
}

Write-Step 'Checking the toolchain'

$required = [ordered]@{
    'Node v24'          = (Join-PathOrNull $node 'node.exe')
    'JDK 17'            = (Join-PathOrNull $jdk 'bin\java.exe')
    'Android SDK'       = $sdk
    'NDK 29.0.13846066' = $ndk
    'zipalign'          = $zipalign
    'apksigner'         = $apksigner
    'debug keystore'    = $keystore
    'generated project' = $project
}
# A discovered path that came back empty is as missing as one that does not
# exist, and Test-Path rejects a null path outright, so both are checked here.
$missing = @($required.GetEnumerator() | Where-Object { -not $_.Value -or -not (Test-Path -LiteralPath $_.Value) })
if ($missing.Count -gt 0) {
    foreach ($item in $missing) {
        $where = if ($item.Value) { $item.Value } else { 'not found' }
        Write-Host ("  missing: {0}`n           {1}" -f $item.Key, $where) -ForegroundColor Red
    }
    throw ('The Android toolchain is incomplete, so the build would fail partway through. ' +
        'Install what is missing above, or point NAPSTR_NODE_DIR, NAPSTR_JDK17, ' +
        'NAPSTR_ANDROID_SDK, NDK_HOME or NAPSTR_BUILD_TOOLS_DIR at it.')
}
foreach ($item in $required.GetEnumerator()) { Write-Host "  ok  $($item.Key)" }

# The build symlinks its own .so into jniLibs, which Windows only allows with
# Developer Mode on. There is no way to make the CLI copy instead, and Gradle
# delegates back to the CLI, so running gradlew directly does not avoid it.
$devMode = 0
try {
    $devMode = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -ErrorAction Stop).AllowDevelopmentWithoutDevLicense
}
catch {
    $devMode = 0
}
if ($devMode -ne 1) {
    throw 'Windows Developer Mode is off. The build needs it to symlink the libraries. Turn it on under Settings > System > For developers.'
}
Write-Host '  ok  Developer Mode'

# Fail here rather than fifteen minutes into a Rust build that runs out of room.
# Only a cold Rust build needs several GB; once the library is built, what is
# left is Gradle's, which needs far less. Demanding the cold figure every time
# would refuse to finish a build that is already most of the way done.
$rustLib = Join-Path $android 'src-tauri\target\aarch64-linux-android\release\libnostrfy_lib.so'
$floorGb = if (Test-Path -LiteralPath $rustLib) { 1.5 } else { 3 }
$freeGb = (Get-PSDrive C).Free / 1GB
Write-Host ('  free space: {0:N2} GB (this build needs about {1:N1} GB)' -f $freeGb, $floorGb)
if ($freeGb -lt $floorGb) {
    throw ('Only {0:N2} GB free, and this build needs about {1:N1} GB. Free space before starting.' -f $freeGb, $floorGb)
}

if ($PreflightOnly) {
    Write-Step 'Preflight only'
    Write-Host '  Everything the build needs is present.' -ForegroundColor Green
    return
}

$env:PATH = "$node;$env:PATH"
$env:JAVA_HOME = $jdk
$env:ANDROID_HOME = $sdk
$env:NDK_HOME = $ndk
# Keeps the debug library slim - 368 MB becomes 82 MB. Only affects the dev
# profile, so it is harmless for a release build.
$env:CARGO_PROFILE_DEV_DEBUG = '0'

if ($ResetGradle) {
    Write-Step 'Resetting Gradle state'
    # The daemon goes first, and this order is the point: a daemon that outlives
    # the cache being cleared keeps its own references to the deleted entries and
    # then fails in seconds with the same hashes, which reads as the fix not
    # working. Every transforms directory is dropped, not just the named hashes,
    # because Gradle truncates the failure list after twelve.
    # Left unredirected on purpose: gradlew writes a deprecation notice to stderr,
    # and piping that through 2>&1 would turn native stderr into error records,
    # which $ErrorActionPreference = 'Stop' would then abort on. Its "1 Daemon
    # stopped" line is worth seeing in the log anyway.
    & (Join-Path $project 'gradlew.bat') --project-dir $project --stop
    Get-ChildItem -LiteralPath (Join-Path $env:USERPROFILE '.gradle\caches') -Directory -ErrorAction SilentlyContinue |
        ForEach-Object {
            Remove-Item -LiteralPath (Join-Path $_.FullName 'transforms') -Recurse -Force -ErrorAction SilentlyContinue
        }
    Write-Host '  stopped the daemon, then dropped the transform cache'
}

$variant = if ($Debug) { 'debug' } else { 'release' }

Write-Step "Building the $variant APK"

# A stale APK in the output directory is how a failed or skipped build looks like
# a successful one, so the directory goes first.
if (Test-Path -LiteralPath $outputs) {
    Remove-Item -LiteralPath $outputs -Recurse -Force
}

$buildArgs = @('--apk', '--target', 'aarch64')
if ($Debug) { $buildArgs = @('--debug') + $buildArgs }

Push-Location $android
try {
    & npm run android:build -- @buildArgs
    if ($LASTEXITCODE -ne 0) {
        throw ("The APK build failed with exit code $LASTEXITCODE. If it failed during configuration " +
            'with "Could not read workspace metadata", re-run with -ResetGradle.')
    }
}
finally {
    Pop-Location
}

$built = @(Get-ChildItem -LiteralPath $outputs -Recurse -Filter '*.apk' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending)
if ($built.Count -eq 0) { throw "The build finished but produced no APK under $outputs" }
$built = $built[0]
Write-Host ("  built {0}  ({1:N1} MB)" -f $built.Name, ($built.Length / 1MB))

if ($NoSign) {
    Write-Step 'Not signing, as asked'
    Write-Host "  unsigned APK: $($built.FullName)"
    Write-Host '  note: an unsigned APK cannot be installed.' -ForegroundColor Yellow
    return
}

Write-Step 'Aligning and signing'

$version = '0.1.0'
if (Test-Path -LiteralPath $configFile) {
    try {
        $parsed = (Get-Content -LiteralPath $configFile -Raw | ConvertFrom-Json).version
        if ($parsed) { $version = $parsed }
    }
    catch {
        # A version this cannot read is not a reason to refuse to name the file.
    }
}
$final = Join-Path ([Environment]::GetFolderPath('Desktop')) "Napstrfy-$version-arm64.apk"
$aligned = Join-Path $env:TEMP 'napstrfy-aligned.apk'

# zipalign must run before signing, and the order is not interchangeable.
& $zipalign -p -f 4 $built.FullName $aligned
if ($LASTEXITCODE -ne 0) { throw 'zipalign failed.' }

# JDK 17's java explicitly: the one on PATH cannot read a PKCS12 keystore.
# --ks-type PKCS12 is required, because apksigner otherwise assumes JKS.
& (Join-Path $jdk 'bin\java.exe') -jar $apksigner sign `
    --ks $keystore `
    --ks-type PKCS12 `
    --ks-pass pass:android `
    --key-pass pass:android `
    --ks-key-alias androiddebugkey `
    --out $final `
    $aligned
if ($LASTEXITCODE -ne 0) { throw 'apksigner failed to sign the APK.' }

Remove-Item -LiteralPath $aligned -Force -ErrorAction SilentlyContinue

Write-Step 'Verifying the signature'
& (Join-Path $jdk 'bin\java.exe') -jar $apksigner verify --print-certs $final
if ($LASTEXITCODE -ne 0) { throw 'The signed APK did not verify.' }

Write-Step 'Done'
Write-Host ("  {0}" -f $final)
Write-Host ("  {0:N2} MB" -f ((Get-Item -LiteralPath $final).Length / 1MB))
Write-Host '  The certificate digest above must match the installed app for this to'
Write-Host '  upgrade in place. A different digest means uninstall, then pair again.'

if ($Install) {
    Write-Step 'Installing over adb'
    if (-not (Test-Path -LiteralPath $adb)) { throw "adb is missing at $adb" }
    $attached = @(& $adb devices | Select-String '\sdevice$')
    if ($attached.Count -eq 0) { throw 'No device is attached, so there is nothing to install onto.' }
    & $adb install -r $final
    if ($LASTEXITCODE -ne 0) { throw 'adb install failed.' }
    Write-Host '  installed'
}
