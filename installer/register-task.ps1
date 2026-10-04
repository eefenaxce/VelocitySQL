# Registers (or re-registers) the task that starts the server at boot.
#
# The installer runs this after copying the binaries, and an administrator can
# run it again later to change the port or the binding:
#
#     powershell -ExecutionPolicy Bypass -File register-task.ps1 `
#         -Exe 'C:\Program Files\VelocitySQL\bin\velocitysql-server.exe' `
#         -Port 5210 -HostBinding 127.0.0.1
#
# A task rather than a Windows service, on purpose: the service control manager
# only talks to a program that implements its protocol, and answers a plain
# console program with error 1053. A task that runs at startup as SYSTEM gives
# the same "the database is up before anyone logs in" result without it.
#
# The scheduled action carries an executable and an argument list as two separate
# values, which is why this is PowerShell and not `schtasks /TR`: a path with
# spaces then needs no quoting at all.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Exe,

    [string] $HostBinding = '127.0.0.1',
    [int] $Port = 5210,
    [string] $Snapshot = "$env:ProgramData\VelocitySQL\data\velocitysql.snapshot",
    [string] $TaskName = 'VelocitySQL',
    [string] $UserId = 'SYSTEM'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Exe)) {
    throw "the server binary is not there: $Exe"
}

$argument = '--host {0} --port {1} --snapshot-path "{2}"' -f $HostBinding, $Port, $Snapshot
$action = New-ScheduledTaskAction -Execute $Exe -Argument $argument -WorkingDirectory (Split-Path -Parent $Snapshot)

if ($UserId -eq 'SYSTEM') {
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
} else {
    $principal = New-ScheduledTaskPrincipal -UserId $UserId -LogonType Interactive -RunLevel Limited
}

$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger (New-ScheduledTaskTrigger -AtStartup) `
    -Principal $principal -Settings $settings -Force | Out-Null

Write-Host "registered the task `"$TaskName`": $Exe $argument"
