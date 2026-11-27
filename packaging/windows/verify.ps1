param(
    [Parameter(Mandatory)][string] $Candidate,
    [Parameter(Mandatory)][string] $CandidateBin,
    [Parameter(Mandatory)][string] $BaselineTag,
    [Parameter(Mandatory)][string] $Evidence
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = (Resolve-Path -LiteralPath "$PSScriptRoot\..\..").Path
$definition = "$PSScriptRoot\Fliuno.wxs"
$profile = Join-Path $Evidence 'profile'
$baseline = Join-Path $Evidence 'baseline'
$installed = Join-Path $env:ProgramFiles 'Fliuno'
$version = (& "$CandidateBin\fliuno.exe" --version) -replace '^fliuno ', ''
$oldVersion = $BaselineTag.Substring(1)
$oldPackage = Join-Path $Evidence 'baseline.msi'
$originalPath = $env:Path

function Invoke-Installer([string] $operation, [string] $package, [string] $step) {
    $log = Join-Path $Evidence "$step.log"
    $process = Start-Process -FilePath msiexec.exe -WindowStyle Hidden -Wait -PassThru `
        -ArgumentList "$operation `"$package`" /qn /norestart /l*v `"$log`""
    if ($process.ExitCode -ne 0) { throw "MSI $step exited $($process.ExitCode). See $log" }
}
function Test-Installation([string] $expected, [string] $source) {
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $env:Path = "$machinePath;$originalPath"
    $command = Get-Command fliuno.exe -CommandType Application | Select-Object -First 1
    if ($command.Source -ne "$installed\fliuno.exe") { throw 'Fliuno is missing from the machine PATH' }
    if ((& fliuno.exe --version) -ne "fliuno $expected") { throw 'Installed version differs' }
    foreach ($name in 'fliuno.exe', 'fliuno-app-server.exe') {
        if ((Get-FileHash "$installed\$name").Hash -ne (Get-FileHash "$source\$name").Hash) {
            throw "Installed $name differs from its build"
        }
    }
}
function Test-Uninstallation {
    $paths = [Environment]::GetEnvironmentVariable('Path', 'Machine').Split(';')
    if (Test-Path -LiteralPath "$installed\fliuno.exe") { throw 'Fliuno launcher is still installed' }
    if (Test-Path -LiteralPath "$installed\fliuno-app-server.exe") { throw 'App-server is still installed' }
    if ($paths.TrimEnd('\') -contains $installed.TrimEnd('\')) { throw 'MSI PATH entry remains' }
    foreach ($file in '.fliuno/config.toml', '.fliuno/credentials/openai.key', '.fliuno/state/workbench.db') {
        if (!(Test-Path -LiteralPath "$profile\$file")) { throw "Profile file was removed: $file" }
    }
}

if ([version]$version -le [version]$oldVersion) { throw 'Candidate must be newer than the baseline' }
if (Test-Path -LiteralPath "$installed\fliuno.exe") { throw 'Run the lifecycle check on a fresh runner' }
New-Item -ItemType Directory -Path $Evidence -Force | Out-Null
python "$root\scripts\verify_installer_profile.py" download $BaselineTag windows $baseline
python "$root\scripts\verify_installer_profile.py" seed $profile "$baseline\fliuno-app-server.exe" $oldVersion
wix build -arch x64 -d "FliunoVersion=$oldVersion" -d "BinDir=$baseline" -out $oldPackage $definition

Invoke-Installer '/i' $Candidate 'clean-install'
Test-Installation $version $CandidateBin
Invoke-Installer '/x' $Candidate 'clean-uninstall'
Test-Uninstallation

Invoke-Installer '/i' $oldPackage 'install-baseline'
Test-Installation $oldVersion $baseline
Invoke-Installer '/i' $Candidate 'upgrade'
Test-Installation $version $CandidateBin
python "$root\scripts\verify_installer_profile.py" verify $profile "$installed\fliuno-app-server.exe" $version

Invoke-Installer '/x' $Candidate 'uninstall'
Test-Uninstallation
Invoke-Installer '/i' $Candidate 'reinstall'
Test-Installation $version $CandidateBin
python "$root\scripts\verify_installer_profile.py" verify $profile "$installed\fliuno-app-server.exe" $version
