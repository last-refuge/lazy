# Builds the Windows release files from a built lazy-desktop.exe (run on Windows):
#   lazy-<version>-windows-x64.zip           unpack anywhere and run lazy.exe
#   Lazy-Setup-<version>-windows-x64.exe     the installer (needs Inno Setup 6: choco install innosetup)
# The files are not signed, so SmartScreen warns the first time ("More info", then "Run anyway").
# usage: packaging\windows\build-windows.ps1 -Binary <lazy-desktop.exe> -Version <version> -OutDir <dir>
param(
  [Parameter(Mandatory = $true)][string]$Binary,
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$OutDir
)
$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = Resolve-Path $OutDir
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("lazy-stage-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path (Join-Path $Stage 'licenses') | Out-Null

Copy-Item $Binary (Join-Path $Stage 'lazy.exe')
Get-ChildItem (Join-Path $Root 'crates\lazy-ui\assets\fonts') -Directory | ForEach-Object {
  $license = Join-Path $_.FullName 'LICENSE.txt'
  if (Test-Path $license) { Copy-Item $license (Join-Path $Stage ("licenses\font-" + $_.Name + "-LICENSE.txt")) }
}

$Zip = Join-Path $OutDir "lazy-$Version-windows-x64.zip"
if (Test-Path $Zip) { Remove-Item $Zip }
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip
Write-Host "wrote $Zip"

$Iscc = (Get-Command iscc -ErrorAction SilentlyContinue).Source
if (-not $Iscc) {
  foreach ($dir in @(${env:ProgramFiles(x86)}, $env:ProgramFiles, "$env:LOCALAPPDATA\Programs")) {
    $candidate = Join-Path $dir 'Inno Setup 6\ISCC.exe'
    if (Test-Path $candidate) { $Iscc = $candidate; break }
  }
}
if (-not $Iscc) { throw 'Inno Setup 6 (ISCC.exe) was not found' }
$Args = @("/DVersion=$Version", "/DSourceDir=$Stage", "/DOutDir=$OutDir")
$Icon = Join-Path $Root 'branding\icons\lazy-app.ico'
if (Test-Path $Icon) { $Args += "/DIconFile=$Icon" }
$Args += (Join-Path $Root 'packaging\windows\lazy.iss')
& $Iscc @Args
if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
Write-Host "wrote the installer in $OutDir"
Remove-Item -Recurse -Force $Stage
