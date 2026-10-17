<#
    endpoint-diff.ps1 - compare two endpoint-snapshot.ps1 JSON files and report
    whether installing/removing the virtual audio driver disturbed any endpoint.

    Usage:
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File endpoint-diff.ps1 `
            -Before research\vad\endpoints-before-driver.json `
            -After  research\vad\endpoints-after-rollback.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Before,
    [Parameter(Mandatory = $true)][string]$After
)
$ErrorActionPreference = 'Continue'
Set-StrictMode -Version Latest

$b = Get-Content $Before -Raw | ConvertFrom-Json
$a = Get-Content $After -Raw | ConvertFrom-Json

function Map($snap) {
    $h = @{}
    foreach ($e in $snap.render) { $h[$e.guid] = $e }
    return $h
}
function Fingerprint($e) {
    $name = ''
    if ($e.pnp) { $name = "$($e.pnp.friendlyName)" }
    return ("state=$($e.deviceState); name=$name; desc=$($e.mmdevicesProps.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6')")
}

$mb = Map $b
$ma = Map $a
$added   = @($ma.Keys | Where-Object { -not $mb.ContainsKey($_) })
$removed = @($mb.Keys | Where-Object { -not $ma.ContainsKey($_) })
$changed = @($ma.Keys | Where-Object { $mb.ContainsKey($_) -and (Fingerprint $mb[$_]) -ne (Fingerprint $ma[$_]) })

Write-Host ("before: {0}  {1} render endpoints, {2} MEDIA devices, secureBoot={3}" -f $Before, $b.renderCount, $b.mediaClassPnp.Count, $b.secureBoot)
Write-Host ("after : {0}  {1} render endpoints, {2} MEDIA devices, secureBoot={3}" -f $After, $a.renderCount, $a.mediaClassPnp.Count, $a.secureBoot)
Write-Host ''
Write-Host ("ADDED   ({0}):" -f $added.Count)
foreach ($g in $added) { Write-Host ("  + {0}  {1}" -f $g, (Fingerprint $ma[$g])) }
Write-Host ("REMOVED ({0}):" -f $removed.Count)
foreach ($g in $removed) { Write-Host ("  - {0}  {1}" -f $g, (Fingerprint $mb[$g])) }
Write-Host ("CHANGED ({0}):" -f $changed.Count)
foreach ($g in $changed) {
    Write-Host ("  ~ {0}" -f $g)
    Write-Host ("      before: " + (Fingerprint $mb[$g]))
    Write-Host ("      after : " + (Fingerprint $ma[$g]))
}

Write-Host ''
Write-Host 'MEDIA class device diff (by instance id):'
$ib = @{}; foreach ($d in $b.mediaClassPnp) { $ib[$d.instanceId] = "$($d.status)/$($d.problem)/$($d.friendlyName)" }
$ia = @{}; foreach ($d in $a.mediaClassPnp) { $ia[$d.instanceId] = "$($d.status)/$($d.problem)/$($d.friendlyName)" }
foreach ($k in @($ia.Keys | Where-Object { -not $ib.ContainsKey($_) })) { Write-Host ("  + {0}  {1}" -f $k, $ia[$k]) }
foreach ($k in @($ib.Keys | Where-Object { -not $ia.ContainsKey($_) })) { Write-Host ("  - {0}  {1}" -f $k, $ib[$k]) }
foreach ($k in @($ia.Keys | Where-Object { $ib.ContainsKey($_) -and $ib[$_] -ne $ia[$_] })) { Write-Host ("  ~ {0}`n      before: {1}`n      after : {2}" -f $k, $ib[$k], $ia[$k]) }

Write-Host ''
Write-Host ("screamPackages: before=[{0}] after=[{1}]" -f ($b.screamPackages -join ' | '), ($a.screamPackages -join ' | '))
Write-Host ("VERDICT: added={0} removed={1} changed={2} -> {3}" -f $added.Count, $removed.Count, $changed.Count,
    $(if ($added.Count -eq 0 -and $removed.Count -eq 0 -and $changed.Count -eq 0) { 'IDENTICAL' } else { 'DIFFERS' }))
