<#
.SYNOPSIS
  Installs the Wren MSIX and checks the behaviours that can break inside the
  MSIX container. Runs the same checks against the plain (unpackaged) exe as a
  control, so a failure can be blamed on the container rather than on the CI
  runner. Meant for a Windows runner or a Windows VM (Windows PowerShell 5.1).

  Results are PASS / FAIL (a requirement) or INFO (an observation to read).
  The script exits non-zero only if a FAIL was recorded; every check always runs.

.EXAMPLE
  ./scripts/msix/Test-WrenMsix.ps1 -Msix msix-out\WrenDictationTest.msix `
    -Cer msix-out\wren-test.cer -ExePath stage\wren-desktop.exe `
    -ModelDir C:\models\parakeet-v3-int8 -OutDir test-out
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Msix,
  [Parameter(Mandatory)][string]$Cer,
  [Parameter(Mandatory)][string]$ExePath,          # unpackaged control binary
  [string]$ModelDir = '',                           # Parakeet dir; empty = skip worker checks
  [string]$GoldenWav = (Join-Path $PSScriptRoot '..\..\tests\fixtures\golden\public\RuidoGravacaoPropria.wav'),
  [string]$GoldenTxt = (Join-Path $PSScriptRoot '..\..\tests\fixtures\golden\public\RuidoGravacaoPropria.txt'),
  [Parameter(Mandatory)][string]$OutDir,
  [string]$PackageName = 'WrenDictationTest',
  [int]$SettleSeconds = 12
)
$ErrorActionPreference = 'Continue'
$null = New-Item -ItemType Directory -Force $OutDir
$OutDir = (Resolve-Path $OutDir).Path
$results = New-Object System.Collections.ArrayList

function Add-Result([string]$Phase, [string]$Check, [string]$Status, [string]$Detail = '') {
  $null = $results.Add([pscustomobject]@{ Phase = $Phase; Check = $Check; Status = $Status; Detail = $Detail })
  Write-Host ("[{0}] {1} / {2}  {3}" -f $Status, $Phase, $Check, $Detail)
}

$roamingWren = Join-Path $env:APPDATA 'wren'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

function Reset-RealState {
  Get-Process wren-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Seconds 1
  if (Test-Path $roamingWren) { Remove-Item $roamingWren -Recurse -Force -ErrorAction SilentlyContinue }
  Get-ItemProperty $runKey -ErrorAction SilentlyContinue | Out-Null
  foreach ($n in (Get-Item $runKey -ErrorAction SilentlyContinue).Property) {
    if ($n -match 'wren') { Remove-ItemProperty $runKey -Name $n -ErrorAction SilentlyContinue }
  }
}

# Seed a completed-onboarding settings.json in the REAL %APPDATA% (what an
# existing NSIS install would leave behind) with DEBUG logging and autostart on.
function Write-SeedSettings {
  $null = New-Item -ItemType Directory -Force $roamingWren
  $settings = @{
    active_provider_id   = 'local-server'
    providers            = @(@{ id = 'local-server'; label = 'Local server'; kind = 'remote_api';
        base_url = 'http://localhost:8555/v1'; api_key = $null; model = 'whisper-1'; sends_audio_externally = $false })
    shortcut             = 'ctrl+shift+space'
    language             = 'pt'
    log_level            = 'debug'
    launch_at_login      = $true
    play_sounds          = $false
    onboarding_completed = $true
  }
  $json = $settings | ConvertTo-Json -Depth 6
  [System.IO.File]::WriteAllText((Join-Path $roamingWren 'settings.json'), $json, (New-Object System.Text.UTF8Encoding($false)))
}

function Save-Screenshot([string]$Path) {
  try {
    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
  } catch { Write-Host "screenshot failed: $_" }
}

function Get-WavPcm([string]$Path) {
  # Returns the raw bytes of the 'data' chunk (expects 16 kHz mono s16le).
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $pos = 12
  while ($pos + 8 -le $bytes.Length) {
    $id = [System.Text.Encoding]::ASCII.GetString($bytes, $pos, 4)
    $len = [BitConverter]::ToUInt32($bytes, $pos + 4)
    if ($id -eq 'data') {
      $n = [Math]::Min([int]$len, $bytes.Length - ($pos + 8))
      $pcm = New-Object byte[] $n
      [Array]::Copy($bytes, $pos + 8, $pcm, 0, $n)
      return , $pcm
    }
    $pos += 8 + $len + ($len % 2)
  }
  throw "no data chunk in $Path"
}

function Get-Words([string]$Text) {
  return ($Text.ToLowerInvariant() -replace '[^\p{L}\p{N}\s]', ' ') -split '\s+' | Where-Object { $_ }
}

# Runs `<exe> __wren-transcribe-worker <model>` with the PCM on stdin.
# stdin comes from a file via cmd.exe redirection on purpose: feeding binary
# data through System.Diagnostics.Process.StandardInput in Windows PowerShell
# 5.1 produced a worker run with empty text (run 3 evidence: same audio, same
# binary, redirect works), so the test harness avoids that pipe.
function Invoke-WorkerRedirect([string]$Exe, [string]$PcmFile, [string]$Tag) {
  $o = Join-Path $OutDir "worker-$Tag.redirect.stdout.txt"
  $e = Join-Path $OutDir "worker-$Tag.redirect.stderr.txt"
  $cmd = "`"`"$Exe`" __wren-transcribe-worker `"$ModelDir`" < `"$PcmFile`" > `"$o`" 2> `"$e`"`""
  & cmd.exe /c $cmd
  return @{ Code = $LASTEXITCODE; Out = (Get-Content $o -Raw -Encoding UTF8); Err = (Get-Content $e -Raw -Encoding UTF8) }
}

function Test-Worker([string]$Phase, [string]$Exe) {
  $tag = $Phase -replace '[^A-Za-z0-9]', '-'
  if (-not $ModelDir -or -not (Test-Path (Join-Path $ModelDir 'encoder-model.int8.onnx'))) {
    Add-Result $Phase 'worker transcription' 'INFO' 'skipped: no model available'
    return
  }
  $pcm = Get-WavPcm $GoldenWav
  $pcmFile = Join-Path $OutDir "input-$tag.pcm"
  [System.IO.File]::WriteAllBytes($pcmFile, $pcm)
  $r = Invoke-WorkerRedirect $Exe $pcmFile $tag
  Remove-Item $pcmFile -ErrorAction SilentlyContinue
  if ($r.Code -ne 0) {
    $err = [string]$r.Err
    Add-Result $Phase 'worker transcription' 'FAIL' ("exit $($r.Code): " + $err.Substring(0, [Math]::Min(400, $err.Length)))
    return
  }
  try { $obj = ([string]$r.Out) | ConvertFrom-Json } catch {
    $o = [string]$r.Out
    Add-Result $Phase 'worker transcription' 'FAIL' "stdout is not one JSON line: $($o.Substring(0, [Math]::Min(200, $o.Length)))"
    return
  }
  $ref = Get-Words (Get-Content $GoldenTxt -Raw -Encoding UTF8)
  $hyp = Get-Words ([string]$obj.text)
  $set = @{}; foreach ($w in $hyp) { $set[$w] = $true }
  $hit = ($ref | Where-Object { $set.ContainsKey($_) }).Count
  $recall = [Math]::Round($hit / [Math]::Max(1, $ref.Count), 2)
  $status = if ($recall -ge 0.8) { 'PASS' } else { 'FAIL' }
  $preview = [string]$obj.text
  if ($preview.Length -gt 80) { $preview = $preview.Substring(0, 80) }
  Add-Result $Phase 'worker transcription' $status "word recall $recall (ref $($ref.Count) words, hyp $($hyp.Count)); load $($obj.load_ms) ms, infer $($obj.infer_ms) ms; text: '$preview'"
}

# Starts the app (with test env vars) and checks the first seconds of life.
function Test-AppLife([string]$Phase, [scriptblock]$Start, [string[]]$DataRoots, [string]$Tag) {
  # The app opens the settings window at boot only with this trigger (no tray in CI).
  $env:WREN_TEST_SETTINGS = '1'
  & $Start
  Remove-Item Env:\WREN_TEST_SETTINGS -ErrorAction SilentlyContinue
  Start-Sleep -Seconds $SettleSeconds

  $proc = Get-Process wren-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($proc) { Add-Result $Phase 'process alive after settle' 'PASS' "pid $($proc.Id)" }
  else { Add-Result $Phase 'process alive after settle' 'FAIL' 'wren-desktop is not running' }

  $title = (Get-Process wren-desktop -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle } |
    Select-Object -First 1).MainWindowTitle
  if ($title) { Add-Result $Phase 'settings window opened' 'PASS' $title }
  else { Add-Result $Phase 'settings window opened' 'FAIL' 'no top-level window title (WebView2 failed or env not passed)' }

  Save-Screenshot (Join-Path $OutDir "screen-$Tag.png")

  # Look in every candidate location and report which one the app really used
  # (inside MSIX the data may be virtualized into the package private store).
  $log = $DataRoots | ForEach-Object { Join-Path $_ 'logs\wren.log' } | Where-Object { Test-Path $_ } | Select-Object -First 1
  if ($log) {
    Add-Result $Phase 'log file written (data dir located)' 'PASS' $log
    $text = Get-Content $log -Raw -ErrorAction SilentlyContinue
    Copy-Item $log (Join-Path $OutDir "wren-$Tag.log") -ErrorAction SilentlyContinue
    if ($text -match 'Wren starting') { Add-Result $Phase 'app logged startup' 'PASS' } else { Add-Result $Phase 'app logged startup' 'FAIL' 'no "Wren starting" line' }
    if ($text -match 'global shortcut registered') { Add-Result $Phase 'global shortcut registered' 'PASS' }
    else {
      $line = ($text -split "`n" | Where-Object { $_ -match 'wren::shortcut' } | Select-Object -First 1)
      Add-Result $Phase 'global shortcut registered' 'FAIL' "$line"
    }
    $auto = ($text -split "`n" | Where-Object { $_ -match 'autostart' })
    if ($auto) { Add-Result $Phase 'autostart log lines' 'INFO' (($auto | Select-Object -First 2) -join ' | ') }
  } else {
    Add-Result $Phase 'log file written (data dir located)' 'FAIL' ("not found in: " + ($DataRoots -join '; '))
  }

  $runValues = @()
  $item = Get-Item $runKey -ErrorAction SilentlyContinue
  if ($item) { $runValues = $item.Property | Where-Object { $_ -match 'wren' } }
  if ($runValues) { Add-Result $Phase 'autostart visible in real HKCU Run' 'INFO' "yes: $($runValues -join ', ')" }
  else { Add-Result $Phase 'autostart visible in real HKCU Run' 'INFO' 'no (expected inside MSIX: registry writes are virtualized)' }

  Get-Process wren-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Seconds 2
}

# ---------------------------------------------------------------- control: unpackaged
Write-Host '=== control: unpackaged exe ==='
Reset-RealState
Write-SeedSettings
Test-AppLife 'control' { Start-Process -FilePath $ExePath } @($roamingWren) 'control'
Test-Worker 'control' $ExePath

# ---------------------------------------------------------------- install the MSIX
Write-Host '=== MSIX install ==='
Reset-RealState
Write-SeedSettings
$pfn = $null
try {
  Import-Certificate -FilePath $Cer -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null
  Add-AppxPackage -Path $Msix -ErrorAction Stop
  $pkg = Get-AppxPackage -Name $PackageName
  if ($pkg) {
    $pfn = $pkg.PackageFamilyName
    Add-Result 'msix' 'install (Add-AppxPackage)' 'PASS' "$($pkg.PackageFullName)"
  } else { Add-Result 'msix' 'install (Add-AppxPackage)' 'FAIL' 'package not listed after install' }
} catch {
  Add-Result 'msix' 'install (Add-AppxPackage)' 'FAIL' "$($_.Exception.Message)"
}

if ($pfn) {
  $pkgRoot = Join-Path $env:LOCALAPPDATA "Packages\$pfn"
  $alias = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\wren.exe'
  $dataRoot = Join-Path $pkgRoot 'LocalCache\Roaming\wren'

  # Launch through the execution alias so the test env vars are inherited.
  $startPackaged = {
    if (Test-Path $alias) { Start-Process -FilePath $alias }
    else { Start-Process "shell:AppsFolder\$pfn!Wren" }
  }
  $realLogsBefore = Test-Path (Join-Path $roamingWren 'logs')
  Add-Result 'msix' 'real %APPDATA% logs absent before launch' $(if ($realLogsBefore) { 'FAIL' } else { 'PASS' }) 'guards against stale files from the control run'
  Test-AppLife 'msix' $startPackaged @($dataRoot, $roamingWren) 'msix'

  # Where did the data really go?
  if (Test-Path (Join-Path $roamingWren 'logs')) {
    Add-Result 'msix' 'logs also in real %APPDATA%' 'INFO' 'yes: writes were NOT redirected'
  } else {
    Add-Result 'msix' 'logs also in real %APPDATA%' 'INFO' 'no: writes redirected to the package private store'
  }
  if (Test-Path $pkgRoot) {
    $size = (Get-ChildItem $pkgRoot -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    Add-Result 'msix' 'package private store' 'INFO' "$pkgRoot ($size bytes)"
  }

  # The hidden worker subcommand must be reachable from inside the package.
  $workerExe = if (Test-Path $alias) { $alias } else { (Get-AppxPackage -Name $PackageName).InstallLocation + '\wren-desktop.exe' }
  Test-Worker 'msix' $workerExe

  # Windows startup task registered from the manifest?
  $startup = Get-ChildItem 'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData' -ErrorAction SilentlyContinue |
    Where-Object { $_.PSChildName -like "$pfn*" }
  Add-Result 'msix' 'startupTask registered in AppModel hive' 'INFO' $(if ($startup) { 'yes' } else { 'not found (may only appear after first activation)' })

  # Uninstall cleanly?
  try {
    Remove-AppxPackage -Package (Get-AppxPackage -Name $PackageName).PackageFullName -ErrorAction Stop
    $left = Get-AppxPackage -Name $PackageName
    Add-Result 'msix' 'uninstall' $(if (-not $left) { 'PASS' } else { 'FAIL' }) ''
  } catch { Add-Result 'msix' 'uninstall' 'FAIL' "$($_.Exception.Message)" }
}

# ---------------------------------------------------------------- crash evidence
try {
  $events = Get-WinEvent -FilterHashtable @{ LogName = 'Application'; StartTime = (Get-Date).AddMinutes(-30) } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'wren' -and $_.LevelDisplayName -in 'Error', 'Warning' } | Select-Object -First 10
  if ($events) {
    $events | Format-List TimeCreated, ProviderName, Id, Message | Out-String | Set-Content (Join-Path $OutDir 'windows-events.txt')
    Add-Result 'evidence' 'Windows Application log' 'INFO' "$(@($events).Count) wren-related error/warning event(s) saved"
  }
} catch { }

# ---------------------------------------------------------------- report
$results | Export-Csv (Join-Path $OutDir 'results.csv') -NoTypeInformation -Encoding UTF8
$icon = @{ PASS = 'PASS'; FAIL = 'FAIL'; INFO = 'info' }
$md = @('## Wren MSIX experiment', '', '| Phase | Check | Result | Detail |', '|---|---|---|---|')
foreach ($r in $results) { $md += "| $($r.Phase) | $($r.Check) | **$($icon[$r.Status])** | $(($r.Detail -replace '\|', '/') -replace "`r?`n", ' ') |" }
$md -join "`n" | Set-Content (Join-Path $OutDir 'summary.md') -Encoding UTF8
if ($env:GITHUB_STEP_SUMMARY) { $md -join "`n" | Add-Content $env:GITHUB_STEP_SUMMARY -Encoding UTF8 }

$failed = @($results | Where-Object { $_.Status -eq 'FAIL' }).Count
Write-Host "Done: $failed failed check(s)."
if ($failed -gt 0) { exit 1 }
