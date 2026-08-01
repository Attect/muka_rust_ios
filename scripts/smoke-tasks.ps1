param(
    [string]$Bin = (Join-Path $PSScriptRoot '..\target\debug\ios.exe')
)
$ErrorActionPreference = 'Stop'

function Get-FreePort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    $listener.Stop()
    return $port
}

function Invoke-Echo([int]$Port, [string]$Text) {
    $client = [System.Net.Sockets.TcpClient]::new()
    $client.Connect('127.0.0.1', $Port)
    $stream = $client.GetStream()
    $stream.ReadTimeout = 3000
    $stream.WriteTimeout = 3000
    $payload = [Text.Encoding]::UTF8.GetBytes($Text)
    $stream.Write($payload, 0, $payload.Length)
    $received = [byte[]]::new($payload.Length)
    $offset = 0
    while ($offset -lt $received.Length) {
        $count = $stream.Read($received, $offset, $received.Length - $offset)
        if ($count -eq 0) { break }
        $offset += $count
    }
    $client.Dispose()
    return [Text.Encoding]::UTF8.GetString($received, 0, $offset)
}

function Read-MjpegFrame([string]$Endpoint) {
    $separator = $Endpoint.LastIndexOf(':')
    if ($separator -lt 1) { throw "invalid MJPEG endpoint: $Endpoint" }
    $client = [Net.Sockets.TcpClient]::new()
    $client.Connect($Endpoint.Substring(0, $separator), [int]$Endpoint.Substring($separator + 1))
    $stream = $client.GetStream()
    $stream.ReadTimeout = 5000
    $request = [Text.Encoding]::ASCII.GetBytes("GET / HTTP/1.1`r`nHost: localhost`r`n`r`n")
    $stream.Write($request, 0, $request.Length)
    $received = [Collections.Generic.List[byte]]::new()
    $buffer = [byte[]]::new(2048)
    $foundJpeg = $false
    $foundHeader = $false
    while ($received.Count -lt 65536 -and (-not $foundJpeg -or -not $foundHeader)) {
        $count = $stream.Read($buffer, 0, $buffer.Length)
        if ($count -eq 0) { break }
        for ($i = 0; $i -lt $count; $i++) { $received.Add($buffer[$i]) }
        $bytes = $received.ToArray()
        $foundHeader = [Text.Encoding]::ASCII.GetString($bytes).Contains('multipart/x-mixed-replace')
        for ($i = 0; $i -lt $bytes.Length - 1; $i++) {
            if ($bytes[$i] -eq 0xff -and $bytes[$i + 1] -eq 0xd8) { $foundJpeg = $true; break }
        }
    }
    $client.Dispose()
    if (-not $foundHeader -or -not $foundJpeg) { throw 'MJPEG response missing header or JPEG frame' }
    return $received.Count
}

function Invoke-CdpCommand([string]$Url, [string]$Json) {
    $websocket = [Net.WebSockets.ClientWebSocket]::new()
    $token = [Threading.CancellationToken]::None
    $websocket.ConnectAsync([Uri]$Url, $token).GetAwaiter().GetResult()
    $payload = [Text.Encoding]::UTF8.GetBytes($Json)
    $websocket.SendAsync(
        [ArraySegment[byte]]::new($payload),
        [Net.WebSockets.WebSocketMessageType]::Text,
        $true,
        $token
    ).GetAwaiter().GetResult()
    $buffer = [byte[]]::new(65536)
    $received = [Collections.Generic.List[byte]]::new()
    do {
        $result = $websocket.ReceiveAsync([ArraySegment[byte]]::new($buffer), $token).GetAwaiter().GetResult()
        for ($i = 0; $i -lt $result.Count; $i++) { $received.Add($buffer[$i]) }
    } while (-not $result.EndOfMessage)
    $websocket.Dispose()
    return [Text.Encoding]::UTF8.GetString($received.ToArray()) | ConvertFrom-Json
}

$Bin = (Resolve-Path -LiteralPath $Bin).Path
$serverPort = Get-FreePort
$serverHome = Join-Path $env:TEMP ("rust-ios-task-smoke-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $serverHome | Out-Null
$env:RUST_IOS_BACKEND = 'mock'
$env:RUST_IOS_MOCK_DEVICES = 'mock-phone'
$common = @('--server-home', $serverHome, '--server-addr', "127.0.0.1:$serverPort")
$debugAppPath = Join-Path $serverHome 'Demo.app'
$lldbCapturePath = Join-Path $serverHome 'debug.lldb.capture'
$fakeLldbPath = Join-Path $serverHome 'fake-lldb.cmd'
New-Item -ItemType Directory -Path $debugAppPath | Out-Null
[IO.File]::WriteAllText((Join-Path $debugAppPath 'Info.plist'), @'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.Demo</string>
<key>CFBundleExecutable</key><string>Demo</string>
</dict></plist>
'@)
[IO.File]::WriteAllText((Join-Path $debugAppPath 'Demo'), 'mock executable')
[IO.File]::WriteAllText(
    $fakeLldbPath,
    "@echo off`r`nif not `"%~1`"==`"-s`" exit /b 9`r`ntype `"%~2`" > `"$lldbCapturePath`"`r`n"
)
$env:RUST_IOS_LLDB = $fakeLldbPath

try {
    $webPages = (& $Bin webinspector list --udid mock-phone @common) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $webPages[0].page.key -ne 'mock-page-1') {
        throw "webinspector list failed"
    }
    $webEval = (& $Bin webinspector eval mock-page-1 '2+2' --udid mock-phone @common) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or [int]$webEval -ne 4) { throw "webinspector eval failed: $webEval" }
    Write-Output "WEBINSPECTOR page=$($webPages[0].page.key) eval=$webEval"

    $forwardRaw = & $Bin forward 0 8100 --udid mock-phone @common
    if ($LASTEXITCODE -ne 0) { throw "forward failed: $forwardRaw" }
    $forward = $forwardRaw | ConvertFrom-Json
    $forwardTask = $forward.value.tasks[0]
    $forwardPort = [int]$forwardTask.spec.configuration.host_port
    Write-Output "FORWARD task=$($forwardTask.id) port=$forwardPort state=$($forwardTask.runtime_state)"

    $echo = Invoke-Echo $forwardPort 'forward-before-restart'
    if ($echo -ne 'forward-before-restart') { throw "bad forward echo: $echo" }
    Write-Output "FORWARD_ECHO_1=$echo"

    $tunnelRaw = & $Bin tunnel start --userspace --userspace-port 0 --udid mock-phone @common
    if ($LASTEXITCODE -ne 0) { throw "tunnel failed: $tunnelRaw" }
    $tunnel = $tunnelRaw | ConvertFrom-Json
    $tunnelTask = $tunnel.value.task
    for ($i = 0; $i -lt 30 -and $tunnelTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $tunnelTask = ((& $Bin task show $tunnelTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($tunnelTask.runtime_state -ne 'running') {
        throw "tunnel not running: $($tunnelTask | ConvertTo-Json -Depth 8)"
    }

    $endpointText = [string]$tunnelTask.runtime.userspaceEndpoint
    $separator = $endpointText.LastIndexOf(':')
    if ($separator -lt 1) { throw "invalid tunnel endpoint: $endpointText" }
    $endpoint = [Net.IPEndPoint]::new(
        [Net.IPAddress]::Parse($endpointText.Substring(0, $separator)),
        [int]$endpointText.Substring($separator + 1)
    )
    $client = [Net.Sockets.TcpClient]::new()
    $client.Connect($endpoint.Address, $endpoint.Port)
    $stream = $client.GetStream()
    $stream.ReadTimeout = 3000
    $stream.WriteTimeout = 3000
    $address = [Net.IPAddress]::IPv6Loopback.GetAddressBytes()
    $stream.Write($address, 0, 16)
    $portBytes = [BitConverter]::GetBytes([uint32]8100)
    $stream.Write($portBytes, 0, 4)
    $payload = [Text.Encoding]::UTF8.GetBytes('tunnel-echo')
    $stream.Write($payload, 0, $payload.Length)
    $received = [byte[]]::new($payload.Length)
    $count = $stream.Read($received, 0, $received.Length)
    $tunnelEcho = [Text.Encoding]::UTF8.GetString($received, 0, $count)
    $client.Dispose()
    if ($tunnelEcho -ne 'tunnel-echo') { throw "bad tunnel echo: $tunnelEcho" }
    Write-Output "TUNNEL task=$($tunnelTask.id) endpoint=$($tunnelTask.runtime.userspaceEndpoint) echo=$tunnelEcho"

    $rsdServices = (& $Bin rsd ls --udid mock-phone @common) | ConvertFrom-Json
    if ($rsdServices.'com.apple.internal.dt.remote.debugproxy'.Port -ne 58785) {
        throw "bad RSD service registry: $($rsdServices | ConvertTo-Json -Depth 8)"
    }
    $rsdServiceCount = @($rsdServices.PSObject.Properties).Count
    Write-Output "RSD_SERVICES count=$rsdServiceCount sharedTunnel=$($tunnelTask.id)"

    $debugSessionOutput = & $Bin debug --stop-at-entry $debugAppPath --udid mock-phone @common
    if ($LASTEXITCODE -ne 0) { throw "debug app session failed: $debugSessionOutput" }
    if ($debugSessionOutput) { throw "debug app session printed unexpected output: $debugSessionOutput" }
    $lldbSource = [IO.File]::ReadAllText($lldbCapturePath)
    $lldbAppPath = $debugAppPath.Replace('\', '/') + '/Demo'
    if (-not $lldbSource.Contains("target create -- `"$lldbAppPath`"") -or
        -not $lldbSource.Contains('/private/var/containers/Bundle/Application/MOCK-APP/Demo.app/Demo') -or
        -not $lldbSource.Contains('process launch --stop-at-entry')) {
        throw "bad LLDB source: $lldbSource"
    }
    $debugTasksAfterSession = ((& $Bin task list --kind debug_proxy @common) | ConvertFrom-Json).value.tasks
    if (@($debugTasksAfterSession).Count -ne 0) {
        throw "debug app session left a proxy task: $($debugTasksAfterSession | ConvertTo-Json -Depth 8)"
    }
    Write-Output "DEBUG_APP bundle=com.example.Demo cleanup=passed sourceBytes=$($lldbSource.Length)"

    $cdpTask = ((& $Bin webinspector cdp --port 0 --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $cdpTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $cdpTask = ((& $Bin task show $cdpTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($cdpTask.runtime_state -ne 'running') { throw 'WebInspector CDP task not running' }
    if ($cdpTask.runtime.serviceTransport -ne 'shared_rsd') { throw 'WebInspector CDP did not reuse shared RSD' }
    $cdpTargets = @(Invoke-RestMethod -Uri $cdpTask.runtime.jsonUrl)
    if ($cdpTargets[0].id -ne 'mock-page-1') { throw 'bad WebInspector CDP discovery' }
    $cdpResponse = Invoke-CdpCommand $cdpTargets[0].webSocketDebuggerUrl '{"id":91,"method":"Runtime.enable"}'
    if ($cdpResponse.id -ne 91 -or $cdpResponse.result.method -ne 'Runtime.enable') {
        throw 'bad WebInspector CDP response'
    }
    Write-Output "WEBINSPECTOR_CDP task=$($cdpTask.id) endpoint=$($cdpTask.runtime.endpoint)"

    $debugProxyTask = ((& $Bin debug proxy --port 0 --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $debugProxyTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $debugProxyTask = ((& $Bin task show $debugProxyTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($debugProxyTask.runtime_state -ne 'running') { throw 'debug proxy task not running' }
    $debugEcho = Invoke-Echo ([int]$debugProxyTask.runtime.port) 'gdb-remote'
    if ($debugEcho -ne 'gdb-remote') { throw "bad debug proxy echo: $debugEcho" }
    Write-Output "DEBUG_PROXY task=$($debugProxyTask.id) endpoint=$($debugProxyTask.runtime.endpoint) echo=$debugEcho"

    $xctestTask = ((& $Bin runtest --bundle-id com.example.Demo --test-runner-bundle-id com.example.DemoUITests.xctrunner --xctest-config DemoUITests.xctest --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $xctestTask.runtime_state -ne 'completed'; $i++) {
        Start-Sleep -Milliseconds 100
        $xctestTask = ((& $Bin task show $xctestTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($xctestTask.runtime_state -ne 'completed' -or $xctestTask.runtime.outcome -ne 'passed') {
        throw "XCTest task did not complete: $($xctestTask | ConvertTo-Json -Depth 8)"
    }
    $xctestOutput = ((& $Bin task output $xctestTask.id @common) | ConvertFrom-Json).value
    $xctestText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($xctestOutput.bytes_base64))
    if (-not $xctestText.Contains('"case_finished"')) { throw 'XCTest output is missing case_finished' }
    & $Bin task restart $xctestTask.id @common | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'XCTest task restart failed' }
    for ($i = 0; $i -lt 30; $i++) {
        $xctestTask = ((& $Bin task show $xctestTask.id @common) | ConvertFrom-Json).value.task
        if ($xctestTask.runtime_state -eq 'completed') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($xctestTask.runtime_state -ne 'completed' -or $xctestTask.generation -ne 2) {
        throw 'XCTest explicit restart did not complete as generation 2'
    }
    Write-Output "XCTEST task=$($xctestTask.id) generation=$($xctestTask.generation) bytes=$($xctestOutput.total_bytes)"

    $appProcessTask = ((& $Bin launch com.example.Demo --wait --kill-existing --arg=--smoke --env=FEATURE=true --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $appProcessTask.runtime_state -ne 'completed'; $i++) {
        Start-Sleep -Milliseconds 100
        $appProcessTask = ((& $Bin task show $appProcessTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($appProcessTask.runtime_state -ne 'completed' -or [int]$appProcessTask.runtime.pid -ne 42424) {
        throw "app process task did not complete: $($appProcessTask | ConvertTo-Json -Depth 8)"
    }
    if ($appProcessTask.runtime.serviceTransport -ne 'shared_rsd') { throw 'app process did not reuse shared RSD' }
    $appProcessOutput = ((& $Bin task output $appProcessTask.id @common) | ConvertFrom-Json).value
    $appProcessText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($appProcessOutput.bytes_base64))
    if ($appProcessText -ne "mock app stdout`nmock app stderr`n") { throw "bad app process output: $appProcessText" }
    Write-Output "APP_PROCESS task=$($appProcessTask.id) pid=$($appProcessTask.runtime.pid) bytes=$($appProcessOutput.total_bytes)"

    $wdaTask = ((& $Bin runwda --bundleid com.example.WebDriverAgentRunner.xctrunner --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $wdaTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $wdaTask = ((& $Bin task show $wdaTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($wdaTask.runtime_state -ne 'running') { throw 'WDA task not running' }
    if ($wdaTask.runtime.wdaUrl -ne 'http://127.0.0.1:18100') { throw 'bad WDA runtime URL' }
    $wdaOutput = ((& $Bin task output $wdaTask.id @common) | ConvertFrom-Json).value
    $wdaText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($wdaOutput.bytes_base64))
    $wdaEvents = @($wdaText.Trim().Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries) | ForEach-Object { $_ | ConvertFrom-Json })
    if ($wdaOutput.content_type -ne 'application/x-ndjson' -or $wdaEvents.Count -ne 1 -or
        $wdaEvents[0].event.kind -ne 'test_plan_started') {
        throw "bad WDA event capture: $wdaText"
    }
    Write-Output "WDA task=$($wdaTask.id) url=$($wdaTask.runtime.wdaUrl) bytes=$($wdaOutput.total_bytes)"

    $syslogTask = ((& $Bin syslog --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $syslogTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $syslogTask = ((& $Bin task show $syslogTask.id @common) | ConvertFrom-Json).value.task
    }
    Start-Sleep -Milliseconds 150
    $syslogOutput = ((& $Bin task output $syslogTask.id @common) | ConvertFrom-Json).value
    $syslogText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($syslogOutput.bytes_base64))
    if (-not $syslogText.Contains('mock syslog mock-phone')) { throw "bad syslog output: $syslogText" }
    Write-Output "SYSLOG task=$($syslogTask.id) bytes=$($syslogOutput.total_bytes)"

    $ostraceTask = ((& $Bin ostrace --pid 42 --level 'Error,fault,ERROR' --subsystem com.example.storage --match timeout --exclude ignored --follow --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $ostraceTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $ostraceTask = ((& $Bin task show $ostraceTask.id @common) | ConvertFrom-Json).value.task
    }
    $ostraceOptions = $ostraceTask.spec.configuration.os_trace
    if (($ostraceOptions.levels -join ',') -ne 'error,fault' -or
        $ostraceOptions.subsystem_contains -ne 'com.example.storage' -or
        $ostraceOptions.message_contains -ne 'timeout' -or
        $ostraceOptions.message_excludes -ne 'ignored' -or
        -not $ostraceOptions.follow) { throw 'bad ostrace task options' }
    Start-Sleep -Milliseconds 150
    $ostraceOutput = ((& $Bin task output $ostraceTask.id @common) | ConvertFrom-Json).value
    $ostraceText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($ostraceOutput.bytes_base64))
    $ostraceEntries = @($ostraceText.Trim().Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries) | ForEach-Object { $_ | ConvertFrom-Json })
    if ($ostraceEntries.Count -eq 0) { throw 'filtered ostrace output is empty' }
    foreach ($entry in $ostraceEntries) {
        if ($entry.level -ne 'Error' -or $entry.label.subsystem -ne 'com.example.storage' -or
            -not $entry.message.Contains('timeout') -or $entry.message.Contains('ignored')) {
            throw "bad filtered ostrace output: $ostraceText"
        }
    }
    Write-Output "OSTRACE task=$($ostraceTask.id) bytes=$($ostraceOutput.total_bytes)"

    $pcapTask = ((& $Bin pcap --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $pcapTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $pcapTask = ((& $Bin task show $pcapTask.id @common) | ConvertFrom-Json).value.task
    }
    Start-Sleep -Milliseconds 150
    $pcapOutput = ((& $Bin task output $pcapTask.id @common) | ConvertFrom-Json).value
    $pcapBytes = [Convert]::FromBase64String($pcapOutput.bytes_base64)
    $pcapMagic = ($pcapBytes[0..3] | ForEach-Object { $_.ToString('x2') }) -join ''
    if ($pcapMagic -ne 'd4c3b2a1') {
        throw 'bad pcap header'
    }
    Write-Output "PCAP task=$($pcapTask.id) bytes=$($pcapOutput.total_bytes)"

    $notificationsTask = ((& $Bin instruments notifications --udid mock-phone @common) | ConvertFrom-Json).value.task
    $sysmontapTask = ((& $Bin sysmontap --udid mock-phone @common) | ConvertFrom-Json).value.task
    foreach ($dvtTask in @($notificationsTask, $sysmontapTask)) {
        for ($i = 0; $i -lt 30 -and $dvtTask.runtime_state -ne 'running'; $i++) {
            Start-Sleep -Milliseconds 100
            $dvtTask = ((& $Bin task show $dvtTask.id @common) | ConvertFrom-Json).value.task
        }
        if ($dvtTask.runtime_state -ne 'running') { throw "DVT stream $($dvtTask.id) not running" }
        if ($dvtTask.runtime.serviceTransport -ne 'shared_rsd') {
            throw "DVT stream did not reuse shared RSD: $($dvtTask | ConvertTo-Json -Depth 8)"
        }
    }
    Start-Sleep -Milliseconds 150
    $notificationsOutput = ((& $Bin task output $notificationsTask.id @common) | ConvertFrom-Json).value
    $sysmontapOutput = ((& $Bin task output $sysmontapTask.id @common) | ConvertFrom-Json).value
    $notificationsText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($notificationsOutput.bytes_base64))
    $sysmontapText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($sysmontapOutput.bytes_base64))
    if (-not $notificationsText.Contains('mock notifications mock-phone')) { throw 'bad notifications output' }
    if (-not $sysmontapText.Contains('mock sysmontap mock-phone')) { throw 'bad sysmontap output' }
    Write-Output "DVT_STREAMS notifications=$($notificationsTask.id) sysmontap=$($sysmontapTask.id)"

    $screenshotTask = ((& $Bin screenshot --stream --port 0 --udid mock-phone @common) | ConvertFrom-Json).value.task
    for ($i = 0; $i -lt 30 -and $screenshotTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $screenshotTask = ((& $Bin task show $screenshotTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($screenshotTask.runtime_state -ne 'running') { throw 'screenshot stream not running' }
    if ($screenshotTask.runtime.serviceTransport -ne 'shared_rsd') { throw 'screenshot stream did not reuse shared RSD' }
    $screenshotEndpoint = [string]$screenshotTask.runtime.endpoint
    $screenshotBytes = Read-MjpegFrame $screenshotEndpoint
    Write-Output "SCREENSHOT_STREAM task=$($screenshotTask.id) endpoint=$screenshotEndpoint bytes=$screenshotBytes"

    $conditionsRaw = & $Bin devicestate list --udid mock-phone @common
    if ($LASTEXITCODE -ne 0) { throw "devicestate list failed: $conditionsRaw" }
    $conditions = $conditionsRaw | ConvertFrom-Json
    if ($conditions[0].Identifier -ne 'SlowNetworkCondition') {
        throw "bad devicestate list: $conditionsRaw"
    }
    $deviceStateRaw = & $Bin devicestate enable SlowNetworkCondition SlowNetwork3GGood --udid mock-phone @common
    if ($LASTEXITCODE -ne 0) { throw "devicestate enable failed: $deviceStateRaw" }
    $deviceStateTask = $deviceStateRaw | ConvertFrom-Json
    for ($i = 0; $i -lt 30 -and $deviceStateTask.runtime_state -ne 'running'; $i++) {
        Start-Sleep -Milliseconds 100
        $deviceStateTask = ((& $Bin task show $deviceStateTask.id @common) | ConvertFrom-Json).value.task
    }
    if ($deviceStateTask.runtime_state -ne 'running') { throw 'devicestate task not running' }
    if ($deviceStateTask.runtime.serviceTransport -ne 'shared_rsd') { throw 'devicestate did not reuse shared RSD' }
    $deviceStateTasks = ((& $Bin task list --kind device_state @common) | ConvertFrom-Json).value.tasks
    if ($deviceStateTasks.Count -ne 1 -or $deviceStateTasks[0].id -ne $deviceStateTask.id) {
        throw 'device_state task filter mismatch'
    }
    Write-Output "DEVICESTATE task=$($deviceStateTask.id) profile=SlowNetwork3GGood"

    & $Bin kill-server @common | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'kill-server failed' }
    Start-Sleep -Milliseconds 600

    $tasksRaw = & $Bin task list @common
    if ($LASTEXITCODE -ne 0) { throw 'restored task list failed' }
    $tasks = $tasksRaw | ConvertFrom-Json
    Write-Output "RESTORED_TASKS=$($tasks.value.tasks.Count)"
    if ($tasks.value.tasks.Count -ne 14) { throw "expected 14 restored tasks" }
    for ($i = 0; $i -lt 30; $i++) {
        $restored = ((& $Bin task show $forwardTask.id @common) | ConvertFrom-Json).value.task
        if ($restored.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restored.runtime_state -ne 'running') { throw 'forward not restored' }
    foreach ($streamTask in @($syslogTask, $ostraceTask, $pcapTask, $notificationsTask, $sysmontapTask)) {
        for ($i = 0; $i -lt 30; $i++) {
            $restoredStream = ((& $Bin task show $streamTask.id @common) | ConvertFrom-Json).value.task
            if ($restoredStream.runtime_state -eq 'running') { break }
            Start-Sleep -Milliseconds 100
        }
        if ($restoredStream.runtime_state -ne 'running') { throw "stream $($streamTask.id) not restored" }
        if ($streamTask.kind -in @('notifications', 'sysmontap') -and $restoredStream.runtime.serviceTransport -ne 'shared_rsd') {
            throw "restored DVT stream did not reuse shared RSD: $($restoredStream | ConvertTo-Json -Depth 8)"
        }
    }
    $restoredOstraceTask = ((& $Bin task show $ostraceTask.id @common) | ConvertFrom-Json).value.task
    $restoredOstraceOptions = $restoredOstraceTask.spec.configuration.os_trace
    if (($restoredOstraceOptions.levels -join ',') -ne 'error,fault' -or
        $restoredOstraceOptions.subsystem_contains -ne 'com.example.storage' -or
        $restoredOstraceOptions.message_contains -ne 'timeout' -or
        $restoredOstraceOptions.message_excludes -ne 'ignored' -or
        -not $restoredOstraceOptions.follow) { throw 'ostrace filters changed after restart' }
    for ($i = 0; $i -lt 30; $i++) {
        $restoredDeviceState = ((& $Bin task show $deviceStateTask.id @common) | ConvertFrom-Json).value.task
        if ($restoredDeviceState.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restoredDeviceState.runtime_state -ne 'running') { throw 'devicestate not restored' }
    if ($restoredDeviceState.runtime.serviceTransport -ne 'shared_rsd') { throw 'restored devicestate did not reuse shared RSD' }
    for ($i = 0; $i -lt 30; $i++) {
        $restoredScreenshot = ((& $Bin task show $screenshotTask.id @common) | ConvertFrom-Json).value.task
        if ($restoredScreenshot.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restoredScreenshot.runtime_state -ne 'running') { throw 'screenshot stream not restored' }
    if ($restoredScreenshot.runtime.serviceTransport -ne 'shared_rsd') { throw 'restored screenshot did not reuse shared RSD' }
    for ($i = 0; $i -lt 30; $i++) {
        $restoredDebugProxy = ((& $Bin task show $debugProxyTask.id @common) | ConvertFrom-Json).value.task
        if ($restoredDebugProxy.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restoredDebugProxy.runtime_state -ne 'running') { throw 'debug proxy not restored' }
    $restoredDebugEcho = Invoke-Echo ([int]$restoredDebugProxy.runtime.port) 'gdb-after-restart'
    if ($restoredDebugEcho -ne 'gdb-after-restart') { throw "bad restored debug proxy echo: $restoredDebugEcho" }
    for ($i = 0; $i -lt 30; $i++) {
        $restoredCdp = ((& $Bin task show $cdpTask.id @common) | ConvertFrom-Json).value.task
        if ($restoredCdp.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restoredCdp.runtime_state -ne 'running') { throw 'WebInspector CDP task not restored' }
    $restoredCdpTargets = @(Invoke-RestMethod -Uri $restoredCdp.runtime.jsonUrl)
    $restoredCdpResponse = Invoke-CdpCommand $restoredCdpTargets[0].webSocketDebuggerUrl '{"id":92,"method":"Runtime.enable"}'
    if ($restoredCdpResponse.id -ne 92) { throw 'restored WebInspector CDP command failed' }
    for ($i = 0; $i -lt 30; $i++) {
        $restoredWda = ((& $Bin task show $wdaTask.id @common) | ConvertFrom-Json).value.task
        if ($restoredWda.runtime_state -eq 'running') { break }
        Start-Sleep -Milliseconds 100
    }
    if ($restoredWda.runtime_state -ne 'running') { throw 'WDA task not restored' }
    if ($restoredWda.runtime.wdaUrl -ne 'http://127.0.0.1:18100') { throw 'bad restored WDA runtime URL' }
    $restoredWdaOutput = ((& $Bin task output $wdaTask.id @common) | ConvertFrom-Json).value
    $restoredWdaText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($restoredWdaOutput.bytes_base64))
    $restoredWdaEvents = @($restoredWdaText.Trim().Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries) | ForEach-Object { $_ | ConvertFrom-Json })
    if ([long]$restoredWdaOutput.total_bytes -le [long]$wdaOutput.total_bytes -or
        $restoredWdaEvents.Count -ne 2) {
        throw "WDA event capture did not append after server restart: $restoredWdaText"
    }
    foreach ($event in $restoredWdaEvents) {
        if ($event.event.kind -ne 'test_plan_started') { throw 'restored WDA capture contains an unexpected event' }
    }
    $restoredXctest = ((& $Bin task show $xctestTask.id @common) | ConvertFrom-Json).value.task
    if ($restoredXctest.runtime_state -ne 'completed' -or $restoredXctest.generation -ne 2 -or $restoredXctest.runtime.outcome -ne 'passed') {
        throw 'finite XCTest task was rerun or lost after server restart'
    }
    $restoredAppProcess = ((& $Bin task show $appProcessTask.id @common) | ConvertFrom-Json).value.task
    if ($restoredAppProcess.runtime_state -ne 'completed' -or $restoredAppProcess.generation -ne 1 -or [int]$restoredAppProcess.runtime.pid -ne 42424) {
        throw 'finite app process task was rerun or lost after server restart'
    }
    $restoredScreenshotBytes = Read-MjpegFrame $screenshotEndpoint
    if ($restoredScreenshotBytes -le 0) { throw 'restored screenshot stream returned no data' }
    $echo2 = Invoke-Echo $forwardPort 'forward-after-restart'
    if ($echo2 -ne 'forward-after-restart') { throw "bad restored echo: $echo2" }
    Start-Sleep -Milliseconds 150
    $restoredSyslog = ((& $Bin task output $syslogTask.id @common) | ConvertFrom-Json).value
    $restoredOstrace = ((& $Bin task output $ostraceTask.id @common) | ConvertFrom-Json).value
    $restoredPcap = ((& $Bin task output $pcapTask.id @common) | ConvertFrom-Json).value
    $restoredNotifications = ((& $Bin task output $notificationsTask.id @common) | ConvertFrom-Json).value
    $restoredSysmontap = ((& $Bin task output $sysmontapTask.id @common) | ConvertFrom-Json).value
    if ([long]$restoredSyslog.total_bytes -le [long]$syslogOutput.total_bytes) { throw 'syslog did not append after restart' }
    if ([long]$restoredOstrace.total_bytes -le [long]$ostraceOutput.total_bytes) { throw 'ostrace did not append after restart' }
    $restoredOstraceText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($restoredOstrace.bytes_base64))
    $restoredOstraceEntries = @($restoredOstraceText.Trim().Split("`n", [System.StringSplitOptions]::RemoveEmptyEntries) | ForEach-Object { $_ | ConvertFrom-Json })
    foreach ($entry in $restoredOstraceEntries) {
        if ($entry.level -ne 'Error' -or $entry.label.subsystem -ne 'com.example.storage' -or
            -not $entry.message.Contains('timeout') -or $entry.message.Contains('ignored')) {
            throw "restored ostrace output violated filters: $restoredOstraceText"
        }
    }
    if ([long]$restoredPcap.total_bytes -le [long]$pcapOutput.total_bytes) { throw 'pcap did not append after restart' }
    if ([long]$restoredNotifications.total_bytes -le [long]$notificationsOutput.total_bytes) { throw 'notifications did not append after restart' }
    if ([long]$restoredSysmontap.total_bytes -le [long]$sysmontapOutput.total_bytes) { throw 'sysmontap did not append after restart' }
    Write-Output "FORWARD_ECHO_2=$echo2"

    & $Bin task stop all @common | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'task stop all failed' }
    & $Bin kill-server @common | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'final kill-server failed' }
    Start-Sleep -Milliseconds 500

    Write-Output '--- STATE ---'
    Get-Content -LiteralPath (Join-Path $serverHome 'state-v1.json')
    Write-Output '--- LOG ---'
    Get-Content -LiteralPath (Join-Path $serverHome 'server.log') -Tail 50
    Write-Output 'PROCESS_SMOKE_OK'
}
finally {
    if (Test-Path -LiteralPath (Join-Path $serverHome 'server.json')) {
        try { & $Bin kill-server @common | Out-Null } catch {}
        Start-Sleep -Milliseconds 300
    }
    $resolvedTemp = [IO.Path]::GetFullPath($env:TEMP)
    $resolvedHome = [IO.Path]::GetFullPath($serverHome)
    if ($resolvedHome.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path $resolvedHome -Leaf).StartsWith('rust-ios-task-smoke-')) {
        Remove-Item -LiteralPath $resolvedHome -Recurse -Force -ErrorAction SilentlyContinue
    }
}
