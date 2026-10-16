param([ValidateSet('filtered', 'context')][string]$DiagnosticMode = 'filtered')
$ErrorActionPreference = 'Stop'
$root = $env:GITHUB_WORKSPACE
$out = Join-Path $env:RUNNER_TEMP 'issue-761-evidence'
$bins = Join-Path $env:RUNNER_TEMP 'issue-761-binaries'
New-Item -ItemType Directory -Force $out, $bins | Out-Null
$dotnetThreads = [Environment]::ProcessorCount
'fn main() { if std::env::args().any(|arg| arg == "--temp") { println!("{}", std::env::temp_dir().display()); } else { println!("{}", std::thread::available_parallelism().unwrap()); } }' | Set-Content "$bins/parallelism.rs"
rustc --crate-name diagnostic_parallelism "$bins/parallelism.rs" -o "$bins/parallelism.exe"
if ($LASTEXITCODE -ne 0) { throw 'parallelism probe build failed' }
$threads = [int](& "$bins/parallelism.exe")
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class DiagnosticIO {
    [StructLayout(LayoutKind.Sequential)]
    public struct Counters {
        public ulong Reads, Writes, Others, ReadBytes, WriteBytes, OtherBytes;
    }
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetProcessIoCounters(IntPtr process, out Counters counters);
}
'@
$summary = [Collections.Generic.List[object]]::new()
$originalTemp = $env:TEMP
$originalTmp = $env:TMP
@{ rust_parallelism = $threads; dotnet_processor_count = $dotnetThreads; temp_env = $env:TEMP; tmp_env = $env:TMP; runner_os = $env:RUNNER_OS; image = $env:ImageVersion;
    rust = (& rustc --version); test_temp = [IO.Path]::GetTempPath(); runner_temp = $env:RUNNER_TEMP;
    volumes = (Get-Volume | Select-Object DriveLetter, Size, SizeRemaining)
} | ConvertTo-Json -Depth 4 | Set-Content "$out/environment.json"
$variants = @(
    @{ name = 'base'; sha = 'a2081bfd109993c76f34ac13b12ee15b48c86008' },
    @{ name = 'head'; sha = '196da2c71e2b7fe18e1f3763d946f19b8791ead5' },
    @{ name = 'accounting-off'; sha = '196da2c71e2b7fe18e1f3763d946f19b8791ead5' }
)
if ($DiagnosticMode -eq 'context') { $variants = @($variants[1], @{ name = 'inactive-fastpath'; sha = $variants[1].sha }) }
foreach ($v in $variants) {
    git checkout --detach $v.sha
    if ($LASTEXITCODE -ne 0) { throw 'checkout failed' }
    $testFile = Join-Path $root 'src/download/orchestration/full/tests.rs'
    $baseBlob = & git rev-parse 'a2081bfd:src/download/orchestration/full/tests.rs'
    $currentBlob = & git rev-parse 'HEAD:src/download/orchestration/full/tests.rs'
    if ($baseBlob -ne $currentBlob) { throw 'worker tests differ from base' }
    if ($DiagnosticMode -eq 'filtered') {
    # Same diagnostic-only bootstrap in every binary; preserve every watchdog/assertion.
    $testText = [IO.File]::ReadAllText($testFile)
    $pattern = '(async fn check_full_sync_worker_budget[^\{]+\{)'
    $bootstrap = @'

    if std::env::var_os("KEI_761_DIAGNOSTIC_TRACING").is_some() {
        tracing_test::internal::INITIALIZED.call_once(|| {
            let writer = tracing_test::internal::MockWriter::new(&tracing_test::internal::global_buf());
            let subscriber = tracing_test::internal::get_subscriber(writer, "kei=trace");
            tracing::dispatcher::set_global_default(subscriber).unwrap();
        });
    }
'@
    if ([regex]::Matches($testText, $pattern).Count -ne 1) { throw 'bootstrap target missing' }
    [IO.File]::WriteAllText($testFile, [regex]::Replace($testText, $pattern, ('$1' + $bootstrap)))
    git diff -- src/download/orchestration/full/tests.rs | Set-Content "$out/bootstrap-$($v.name).patch"
    }
    if ($v.name -eq 'accounting-off') {
        $file = Join-Path $root 'src/test_helpers/allocation.rs'
        $s = [IO.File]::ReadAllText($file)
        $start = $s.IndexOf('fn record(')
        $end = $s.IndexOf('// SAFETY:', $start)
        if ($start -lt 0 -or $end -lt $start) { throw 'accounting patch target missing' }
        $s = $s.Replace('#[derive(Clone, Copy, Default)]', '#[allow(dead_code, reason = "diagnostic accounting control retains the original measurement layout")]' + "`n" + '#[derive(Clone, Copy, Default)]')
        $s = $s.Substring(0, $s.IndexOf('fn record(')) + "fn record(_allocated: usize, _released: usize) {}`n`n" + $s.Substring($s.IndexOf('// SAFETY:', $s.IndexOf('fn record(')))
        [IO.File]::WriteAllText($file, $s)
        git diff -- src/test_helpers/allocation.rs | Set-Content "$out/accounting-off.patch"
    }
    if ($v.name -eq 'inactive-fastpath') {
        python "$env:RUNNER_TEMP/diagnose-761-context.py" patch (Join-Path $root 'src/test_helpers/allocation.rs')
        if ($LASTEXITCODE -ne 0) { throw 'fast-path patch failed' }
        git diff -- src/test_helpers/allocation.rs | Set-Content "$out/inactive-fastpath.patch"
    }
    cargo test --locked --all-features --lib --no-run --message-format=json 1> "$out/build-$($v.name).jsonl" 2> "$out/build-$($v.name).log"
    if ($LASTEXITCODE -ne 0) { throw "build failed: $($v.name)" }
    $artifact = Get-Content "$out/build-$($v.name).jsonl" | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'kei' -and $_.profile.test -and $_.executable } |
        Select-Object -Last 1
    if (!$artifact) { throw 'library test binary not found' }
    Copy-Item $artifact.executable "$bins/$($v.name).exe"
    $filter = if ($DiagnosticMode -eq 'filtered') { @('full_sync_worker_budget') } else { @() }
    $catalog = @(& "$bins/$($v.name).exe" @filter --list | Where-Object { $_ -match ': test$' } | Sort-Object)
    $catalog | Set-Content "$out/catalog-$($v.name).txt"
    if (@($catalog | Where-Object { $_ -match 'full_sync_worker_budget' }).Count -ne 4) { throw 'expected exactly four worker-budget tests' }
    if ($v -eq $variants[0]) { $baseCatalog = $catalog -join "`n" }
    elseif (($catalog -join "`n") -ne $baseCatalog) { throw 'test catalogs differ' }
    @{ variant = $v.name; source = $v.sha; original_test_blob = $currentBlob; diagnostic_test_sha256 = (Get-FileHash $testFile -Algorithm SHA256).Hash;
        binary_sha256 = (Get-FileHash "$bins/$($v.name).exe" -Algorithm SHA256).Hash
    } | ConvertTo-Json | Set-Content "$out/build-$($v.name)-provenance.json"
    git restore -- src/download/orchestration/full/tests.rs
    if ($v.name -in @('accounting-off', 'inactive-fastpath')) { git restore -- src/test_helpers/allocation.rs }
}
if ($DiagnosticMode -eq 'context') {
    python "$env:RUNNER_TEMP/diagnose-761-context.py" catalog $out
    if ($LASTEXITCODE -ne 0) { throw 'context catalog selection failed' }
    $contextArgs = @(Get-Content "$out/context-args.json" -Raw | ConvertFrom-Json)
    Get-Content "$out/context-selection.json" | Write-Host
}
function Measure-DiskControl([string]$label) {
    $dir = Join-Path ([IO.Path]::GetTempPath()) ("kei-761-disk-$label-" + [guid]::NewGuid())
    New-Item -ItemType Directory $dir | Out-Null
    $times = @()
    $bytes = [byte[]]::new(32)
    for ($i = 0; $i -lt 32; $i++) {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $path = Join-Path $dir "$i.bin"
        $stream = [IO.File]::Open($path, 'CreateNew', 'Write', 'None')
        try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
        $times += $clock.Elapsed.TotalMilliseconds
        Remove-Item $path
    }
    Remove-Item $dir
    $sorted = @($times | Sort-Object)
    return @{ median_ms = $sorted[16]; p95_ms = $sorted[30]; total_ms = ($times | Measure-Object -Sum).Sum }
}
function Invoke-Comparison([string]$variant, [int]$round, [bool]$traced, [string]$volume = 'C') {
    $mode = if ($traced) { 'traced' } else { 'untraced' }
    $env:KEI_761_DIAGNOSTIC_TRACING = if ($traced) { '1' } else { $null }
    $label = "$mode-$round-$variant"
    $trialTemp = $null
    if ($DiagnosticMode -eq 'context') {
        $mode = "context-$volume"
        $label = "$mode-$round-$variant"
        if ($volume -eq 'D') {
            $trialTemp = Join-Path $env:RUNNER_TEMP ("kei761-" + [guid]::NewGuid().ToString('N'))
            New-Item -ItemType Directory $trialTemp | Out-Null
            $env:TEMP = $trialTemp
            $env:TMP = $trialTemp
        }
    }
    $actualTemp = & "$bins/parallelism.exe" --temp
    if ($DiagnosticMode -eq 'context' -and !$actualTemp.StartsWith("$volume`:", [StringComparison]::OrdinalIgnoreCase)) { throw 'child temp volume was not applied' }
    $arguments = if ($DiagnosticMode -eq 'context') { $contextArgs + @("--test-threads=$threads") } else { @('full_sync_worker_budget', "--test-threads=$threads") }
    $disk = Measure-DiskControl $label
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process "$bins/$variant.exe" -ArgumentList $arguments `
        -PassThru -RedirectStandardOutput "$out/$label.stdout.log" -RedirectStandardError "$out/$label.stderr.log"
    $null = $p.Handle
    $samples = [Collections.Generic.List[object]]::new()
    $cpu = 0.0
    $killed = $false
    $io = [DiagnosticIO+Counters]::new()
    while (!$p.WaitForExit(500)) {
        $p.Refresh()
        try { $cpu = [Math]::Max($cpu, $p.TotalProcessorTime.TotalSeconds) } catch { }
        [void][DiagnosticIO]::GetProcessIoCounters($p.Handle, [ref]$io)
        $samples.Add(@{ wall_s = $clock.Elapsed.TotalSeconds; cpu_s = $cpu;
            read_bytes = $io.ReadBytes; write_bytes = $io.WriteBytes; read_ops = $io.Reads; write_ops = $io.Writes })
        if ($clock.Elapsed.TotalSeconds -gt $(if ($DiagnosticMode -eq 'context') { 420 } else { 180 })) {
            & taskkill.exe /PID $p.Id /T /F | Out-Null
            $killed = $true
            break
        }
    }
    $p.WaitForExit()
    try { $cpu = [Math]::Max($cpu, $p.TotalProcessorTime.TotalSeconds) } catch { }
    [void][DiagnosticIO]::GetProcessIoCounters($p.Handle, [ref]$io)
    $row = @{ variant = $variant; round = $round; traced = $traced; mode = $mode; label = $label; temp = $env:TEMP; rust_temp = $actualTemp; dotnet_temp = [IO.Path]::GetTempPath(); threads = $threads; exit = $p.ExitCode; killed = $killed;
        read_bytes = $io.ReadBytes; write_bytes = $io.WriteBytes; read_ops = $io.Reads; write_ops = $io.Writes;
        wall_s = $clock.Elapsed.TotalSeconds; cpu_s = $cpu; disk = $disk }
    $summary.Add($row)
    $samples | ConvertTo-Json -Depth 4 | Set-Content "$out/$label.samples.json"
    $summary | ConvertTo-Json -Depth 5 | Set-Content "$out/summary.json"
    $row | ConvertTo-Json -Depth 4 -Compress | Write-Host
    Get-Content "$out/$label.stdout.log" | Select-Object -Last 8 | Write-Host
    if ($trialTemp) {
        $env:TEMP = $originalTemp
        $env:TMP = $originalTmp
        try { Remove-Item -LiteralPath $trialTemp -Recurse -Force } catch { Write-Warning "owned trial directory retained: $trialTemp" }
    }
}
# Filtered trials rotate variants; context trials retain natural tracing and pre-failure work.
if ($DiagnosticMode -eq 'context') {
    Invoke-Comparison 'head' 1 $false 'C'
    Invoke-Comparison 'inactive-fastpath' 2 $false 'C'
    Invoke-Comparison 'head' 3 $false 'D'
    Invoke-Comparison 'head' 4 $false 'D'
    Invoke-Comparison 'inactive-fastpath' 5 $false 'C'
    Invoke-Comparison 'head' 6 $false 'C'
} else {
foreach ($traced in @($false, $true)) {
foreach ($round in 1..3) {
    $order = switch ($round) {
        1 { @('base', 'head', 'accounting-off') }
        2 { @('accounting-off', 'base', 'head') }
        3 { @('head', 'accounting-off', 'base') }
    }
    foreach ($v in $order) { Invoke-Comparison $v $round $traced }
}
# Diagnostic variants intentionally preserve failures for analysis rather than hide them.

}

}
