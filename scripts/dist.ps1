# Build release binaries for the local agent and write SHA-256 checksums.
# Usage: powershell -File scripts/dist.ps1 [TargetDir]
# The bash counterpart is dist.sh; both must stay behaviorally identical.
$ErrorActionPreference = "Stop"

# PACKAGE-01: a native command's non-zero exit is a failure here — cargo
# build must never be followed by "package whatever was already there".
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock]$Command)
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

$TargetDir = if ($args.Count -gt 0) { $args[0] } else { "target" }
$VersionLine = Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.*)"' |
    Select-Object -First 1
$Version = $VersionLine.Matches[0].Groups[1].Value
$Out = "dist/$Version"

# PACKAGE-01: build INTO the same directory the artifacts are copied from
# (--target-dir), so a custom target can never mix stale artifacts from an
# older default-target build into this package.
Invoke-Native { cargo build --release --bin agent-tui --target-dir $TargetDir }
Invoke-Native { cargo build --release --bin agent-host --target-dir $TargetDir }

# PACKAGE-01: clean staging — stale files from a previous run (an old
# context-service binary, an old desktop publish) must never ride along.
Remove-Item -Recurse -Force $Out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Copy-Item "$TargetDir/release/agent-tui.exe" "$Out/agent-tui.exe"
Copy-Item "$TargetDir/release/agent-host.exe" "$Out/agent-host.exe"
if (Test-Path "$TargetDir/release/agent-context-service.exe") {
    Invoke-Native { cargo build --release --bin agent-context-service --target-dir $TargetDir }
    Copy-Item "$TargetDir/release/agent-context-service.exe" "$Out/"
}

# Source identity (R1): what this package is built from, when, and by which
# toolchain. Checksums tie each artifact to this exact build.
$SourceSha = git rev-parse HEAD 2>$null
if (-not $SourceSha) { $SourceSha = "unknown (no git checkout)" }
$SourceLines = @(
    "repository: context-agent-prototype",
    "rust_source_sha: $SourceSha",
    "cargo_version: $Version",
    "built_at: $([DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'))"
)
Set-Content -Path "$Out/SOURCE.txt" -Value $SourceLines

# The desktop app when the .NET SDK is present; otherwise the package says
# so and the native binaries above remain the deliverable.
if (Get-Command dotnet -ErrorAction SilentlyContinue) {
    Invoke-Native { dotnet publish apps/Agent.Desktop/Agent.Desktop.csproj -c Release -o "$Out/desktop" }
    Add-Content -Path "$Out/SOURCE.txt" -Value "dotnet_sdk: $(& dotnet --version)"
} else {
    Add-Content -Path "$Out/SOURCE.txt" -Value "desktop: NOT_RUN (no .NET SDK available)"
}

$Checksums = Join-Path $Out "SHA256SUMS"
Remove-Item $Checksums -ErrorAction SilentlyContinue
$OutRoot = (Resolve-Path $Out).Path
Get-ChildItem $Out -Recurse -File | Where-Object { $_.Name -ne "SHA256SUMS" } | ForEach-Object {
    $Hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
    $Relative = $_.FullName.Substring($OutRoot.Length + 1).Replace("\", "/")
    Add-Content $Checksums "$Hash  $Relative"
}
Write-Output "dist ready: $Out"
Get-Content $Checksums
