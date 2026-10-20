$p = Get-Process claude-relay -ErrorAction SilentlyContinue
if ($p) {
    '内存(MB): ' + [math]::Round($p.WorkingSet64 / 1MB, 1)
    'CPU时间(s): ' + [math]::Round($p.CPU, 3)
    '线程数: ' + $p.Threads.Count
    '句柄数: ' + $p.HandleCount
} else {
    'claude-relay not running'
}
