<#
.SYNOPSIS
  Packs the built Wren binary into an MSIX and signs it with a throwaway
  self-signed certificate (test use only; the Store re-signs real submissions).

.EXAMPLE
  ./scripts/msix/New-WrenMsix.ps1 -ExeDir stage -OutDir msix-out
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$ExeDir,        # folder with wren-desktop.exe (+ any DLLs)
  [Parameter(Mandatory)][string]$OutDir,
  [string]$ExeName = 'wren-desktop.exe',
  [string]$PackageName = 'WrenDictationTest',
  [string]$Publisher = 'CN=Wren Test',
  [string]$IconsDir = (Join-Path $PSScriptRoot '..\..\apps\desktop\src-tauri\icons')
)
$ErrorActionPreference = 'Stop'

function Find-SdkTool([string]$Name) {
  $root = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
  $hit = Get-ChildItem $root -Recurse -Filter $Name -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } |
    Sort-Object FullName -Descending | Select-Object -First 1
  if (-not $hit) { throw "$Name not found under $root (install the Windows SDK)" }
  return $hit.FullName
}

$makeappx = Find-SdkTool 'makeappx.exe'
$signtool = Find-SdkTool 'signtool.exe'

# MSIX versions are 4-part; take the workspace version and append .0.
$cargo = Get-Content (Join-Path $PSScriptRoot '..\..\Cargo.toml') -Raw
if ($cargo -notmatch '(?m)^version = "(\d+\.\d+\.\d+)"') { throw 'workspace version not found in Cargo.toml' }
$version = "$($Matches[1]).0"

New-Item -ItemType Directory -Force $OutDir | Out-Null
$layout = Join-Path $OutDir 'layout'
if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $layout 'Assets') | Out-Null

# 1. Layout: binary (+ DLLs) and the logo assets referenced by the manifest.
Copy-Item (Join-Path $ExeDir $ExeName) $layout
Get-ChildItem $ExeDir -Filter *.dll -ErrorAction SilentlyContinue | Copy-Item -Destination $layout
foreach ($a in 'Square44x44Logo.png', 'Square150x150Logo.png', 'StoreLogo.png') {
  Copy-Item (Join-Path $IconsDir $a) (Join-Path $layout 'Assets')
}

# 2. Manifest from template.
$manifest = Get-Content (Join-Path $PSScriptRoot 'AppxManifest.template.xml') -Raw
$manifest = $manifest.Replace('{{NAME}}', $PackageName).Replace('{{PUBLISHER}}', $Publisher).
  Replace('{{VERSION}}', $version).Replace('{{EXE}}', $ExeName)
[System.IO.File]::WriteAllText((Join-Path $layout 'AppxManifest.xml'), $manifest,
  (New-Object System.Text.UTF8Encoding($false)))

# 3. Throwaway code-signing certificate. Subject MUST equal the manifest Publisher.
$cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
  -KeyUsage DigitalSignature -FriendlyName 'Wren MSIX test' `
  -CertStoreLocation 'Cert:\CurrentUser\My' `
  -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
$cerPath = Join-Path $OutDir 'wren-test.cer'
Export-Certificate -Cert $cert -FilePath $cerPath | Out-Null   # public part only

# 4. Pack + sign (signing by thumbprint from the user store: no password on argv).
$msix = Join-Path $OutDir "$PackageName.msix"
& $makeappx pack /d $layout /p $msix /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)" }
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix
if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)" }

Write-Host "MSIX: $msix (version $version)"
Write-Host "CER : $cerPath"
