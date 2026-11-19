
$ErrorActionPreference = 'Stop'
if ([Console]::IsOutputRedirected -or [Console]::IsInputRedirected) { throw 'CONSOLE_REDIRECTED' }
[Console]::WriteLine('CONSOLE_OK')
[Console]::WriteLine(('SIZE:{0}:{1}' -f [Console]::WindowHeight,[Console]::WindowWidth))
# The descendant only exercises Job Object cleanup. Start it directly without
# attaching another PowerShell process to the fixture's console.
$start = [Diagnostics.ProcessStartInfo]::new(
    'powershell.exe', '-NoLogo -NoProfile -NonInteractive -Command "[Threading.Thread]::Sleep(600000)"')
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$child = [Diagnostics.Process]::Start($start)
$workspace = $PWD.Path
[IO.File]::WriteAllText([IO.Path]::Combine($workspace, 'descendant.pid'), $child.Id.ToString())
$child.Dispose()
[IO.File]::WriteAllText([IO.Path]::Combine($workspace, 'shell.pid'), $PID.ToString())
[Console]::WriteLine('READY')
while ($null -ne ($line = [Console]::ReadLine())) {
    [IO.File]::AppendAllText([IO.Path]::Combine($workspace, 'received-lines'), $line + "`n")
    [Console]::WriteLine('echo:' + $line)
    [Console]::WriteLine(('SIZE:{0}:{1}' -f [Console]::WindowHeight,[Console]::WindowWidth))
    if ($line -eq 'exit') { break }
}
