# Build release binaries for the local agent and write SHA-256 checksums.
# Usage: powershell -File scripts/dist.ps1 [TargetDir]
# The bash counterpart is dist.sh; both must stay behaviorally identical.
$ErrorActionPreference = "Stop"

$TargetDir = if ($args.Count -gt 0) { $args[0] } else { "target" }
$VersionLine = Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.*)"' |
    Select-Object -First 1
$Version = $VersionLine.Matches[0].Groups[1].Value
$Out = "dist/$Version"

cargo build --release --bin agent-tui
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Copy-Item "$TargetDir/release/agent-tui.exe" "$Out/agent-tui.exe"
if (Test-Path "$TargetDir/release/agent-context-service.exe") {
    cargo build --release --bin agent-context-service
    Copy-Item "$TargetDir/release/agent-context-service.exe" "$Out/"
}

$Checksums = Join-Path $Out "SHA256SUMS"
Remove-Item $Checksums -ErrorAction SilentlyContinue
Get-ChildItem $Out -File | Where-Object { $_.Name -ne "SHA256SUMS" } | ForEach-Object {
    $Hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
    Add-Content $Checksums "$Hash  $($_.Name)"
}
Write-Output "dist ready: $Out"
Get-Content $Checksums
