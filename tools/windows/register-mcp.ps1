<#
.SYNOPSIS
    Registers or removes RuStudio's OmniMCP stdio bridge (`rustudio-mcp.exe`)
    in Windows AI coding client MCP configurations.

.DESCRIPTION
    Locates `rustudio-mcp.exe` (next to this script, in `%LOCALAPPDATA%\Programs\rustudio`,
    in `target\{release,agent,debug}`, or on `PATH`) and safely merges an entry for
    `RuStudio_OmniMCP` into the JSON MCP configuration files for:
      - Antigravity   (%USERPROFILE%\.gemini\antigravity\mcp_config.json)
      - ClaudeDesktop (%APPDATA%\Claude\claude_desktop_config.json)
      - ClaudeCode    (%USERPROFILE%\.claude.json)
      - Cursor        (%USERPROFILE%\.cursor\mcp.json)
      - VSCode        (%APPDATA%\Code\User\mcp.json)
      - Windsurf      (%USERPROFILE%\.codeium\windsurf\mcp_config.json)

.PARAMETER Targets
    One or more client names (`All`, `Antigravity`, `ClaudeDesktop`, `ClaudeCode`,
    `Cursor`, `VSCode`, `Windsurf`). Defaults to `All` (updates installed/existing
    client configs; pass a specific target name or `-CreateMissing` to create new ones).

.PARAMETER BinaryPath
    Explicit path to `rustudio-mcp.exe`. Auto-detected when omitted.

.PARAMETER ServerName
    MCP server key name in JSON config. Defaults to `RuStudio_OmniMCP`.

.PARAMETER DryRun
    Preview changes without creating directories or modifying any files.

.PARAMETER Uninstall
    Remove the `RuStudio_OmniMCP` entry from target MCP configurations.

.PARAMETER CreateMissing
    When `-Targets All` is used, create configuration files even for clients whose
    config folder does not exist yet.
#>
[CmdletBinding()]
param(
    [string[]]$Targets = @('All'),
    [string]$BinaryPath,
    [string]$ServerName = 'RuStudio_OmniMCP',
    [switch]$DryRun,
    [switch]$Uninstall,
    [switch]$CreateMissing
)

$ErrorActionPreference = 'Stop'

function Resolve-McpBinary {
    param([string]$ExplicitPath)

    if ($ExplicitPath) {
        $resolved = [IO.Path]::GetFullPath($ExplicitPath)
        if (!(Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "Specified rustudio-mcp.exe not found: $resolved"
        }
        return $resolved
    }

    $repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
    $candidates = @(
        (Join-Path $PSScriptRoot 'rustudio-mcp.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\rustudio\rustudio-mcp.exe'),
        (Join-Path $repoRoot 'target\release\rustudio-mcp.exe'),
        (Join-Path $repoRoot 'target\agent\rustudio-mcp.exe'),
        (Join-Path $repoRoot 'target\debug\rustudio-mcp.exe')
    )

    foreach ($cand in $candidates) {
        if (Test-Path -LiteralPath $cand -PathType Leaf) {
            return [IO.Path]::GetFullPath($cand)
        }
    }

    $onPath = Get-Command 'rustudio-mcp.exe' -ErrorAction SilentlyContinue
    if ($onPath -and $onPath.Source) {
        return [IO.Path]::GetFullPath($onPath.Source)
    }

    throw "Could not locate rustudio-mcp.exe. Build it with 'cargo build --release --bin rustudio-mcp', install RuStudio, or pass -BinaryPath."
}

function Normalize-TargetNames {
    param([string[]]$RawTargets)

    $allCanonical = @('Antigravity', 'ClaudeDesktop', 'ClaudeCode', 'Cursor', 'VSCode', 'Windsurf')
    $expanded = New-Object System.Collections.Generic.List[string]
    foreach ($item in $RawTargets) {
        foreach ($part in ($item -split ',')) {
            $t = $part.Trim()
            if (!$t) { continue }
            switch ($t.ToLowerInvariant()) {
                'all'            { foreach ($c in $allCanonical) { if (!$expanded.Contains($c)) { $expanded.Add($c) } }; break }
                'antigravity'    { if (!$expanded.Contains('Antigravity'))   { $expanded.Add('Antigravity') }; break }
                'claudedesktop'  { if (!$expanded.Contains('ClaudeDesktop')) { $expanded.Add('ClaudeDesktop') }; break }
                'claude-desktop' { if (!$expanded.Contains('ClaudeDesktop')) { $expanded.Add('ClaudeDesktop') }; break }
                'claude_desktop' { if (!$expanded.Contains('ClaudeDesktop')) { $expanded.Add('ClaudeDesktop') }; break }
                'claudecode'     { if (!$expanded.Contains('ClaudeCode'))    { $expanded.Add('ClaudeCode') }; break }
                'claude-code'    { if (!$expanded.Contains('ClaudeCode'))    { $expanded.Add('ClaudeCode') }; break }
                'claude_code'    { if (!$expanded.Contains('ClaudeCode'))    { $expanded.Add('ClaudeCode') }; break }
                'cursor'         { if (!$expanded.Contains('Cursor'))        { $expanded.Add('Cursor') }; break }
                'vscode'         { if (!$expanded.Contains('VSCode'))        { $expanded.Add('VSCode') }; break }
                'vs-code'        { if (!$expanded.Contains('VSCode'))        { $expanded.Add('VSCode') }; break }
                'windsurf'       { if (!$expanded.Contains('Windsurf'))      { $expanded.Add('Windsurf') }; break }
                default {
                    throw "Unknown target '$t'. Valid targets: All, Antigravity, ClaudeDesktop, ClaudeCode, Cursor, VSCode, Windsurf."
                }
            }
        }
    }
    return $expanded.ToArray()
}

function Get-ClientCatalog {
    return @{
        'Antigravity' = @{
            Path      = (Join-Path $env:USERPROFILE '.gemini\antigravity\mcp_config.json')
            DetectDir = (Join-Path $env:USERPROFILE '.gemini\antigravity')
            RootKey   = 'mcpServers'
            UseStdio  = $false
        }
        'ClaudeDesktop' = @{
            Path      = (Join-Path $env:APPDATA 'Claude\claude_desktop_config.json')
            DetectDir = (Join-Path $env:APPDATA 'Claude')
            RootKey   = 'mcpServers'
            UseStdio  = $false
        }
        'ClaudeCode' = @{
            Path      = (Join-Path $env:USERPROFILE '.claude.json')
            DetectDir = (Join-Path $env:USERPROFILE '.claude')
            RootKey   = 'mcpServers'
            UseStdio  = $false
        }
        'Cursor' = @{
            Path      = (Join-Path $env:USERPROFILE '.cursor\mcp.json')
            DetectDir = (Join-Path $env:USERPROFILE '.cursor')
            RootKey   = 'mcpServers'
            UseStdio  = $false
        }
        'VSCode' = @{
            Path      = (Join-Path $env:APPDATA 'Code\User\mcp.json')
            DetectDir = (Join-Path $env:APPDATA 'Code\User')
            RootKey   = 'servers'
            UseStdio  = $true
        }
        'Windsurf' = @{
            Path      = (Join-Path $env:USERPROFILE '.codeium\windsurf\mcp_config.json')
            DetectDir = (Join-Path $env:USERPROFILE '.codeium\windsurf')
            RootKey   = 'mcpServers'
            UseStdio  = $false
        }
    }
}

function Write-Utf8NoBom {
    param(
        [string]$Path,
        [string]$Content
    )
    $parent = Split-Path -Parent $Path
    if ($parent -and !(Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $tmp = "$Path.tmp.$PID"
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($tmp, $Content, $utf8NoBom)
    Move-Item -LiteralPath $tmp -Destination $Path -Force
}

function Read-JsonObject {
    param([string]$Path)
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) {
        return New-Object PSCustomObject
    }
    $raw = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
    if ([string]::IsNullOrWhiteSpace($raw)) {
        return New-Object PSCustomObject
    }
    $parsed = $raw | ConvertFrom-Json
    if ($null -eq $parsed -or $parsed -isnot [PSCustomObject]) {
        throw "Config file '$Path' does not contain a JSON object at top level."
    }
    return $parsed
}

$targetList = Normalize-TargetNames -RawTargets $Targets
$explicitSelection = !($Targets.Count -eq 1 -and $Targets[0].Trim().Equals('All', [StringComparison]::OrdinalIgnoreCase))
$catalog = Get-ClientCatalog

$exePath = $null
if (!$Uninstall) {
    $exePath = Resolve-McpBinary -ExplicitPath $BinaryPath
    Write-Host "Using rustudio-mcp.exe: $exePath"
}

$results = New-Object System.Collections.Generic.List[object]

foreach ($name in $targetList) {
    $info = $catalog[$name]
    $cfgPath = $info.Path
    $rootKey = $info.RootKey
    $exists = Test-Path -LiteralPath $cfgPath -PathType Leaf

    if ($Uninstall) {
        if (!$exists) {
            $results.Add([PSCustomObject]@{ Target = $name; Status = 'Skipped (not found)'; Path = $cfgPath })
            continue
        }
        $rootObj = Read-JsonObject -Path $cfgPath
        $mapProp = $rootObj.PSObject.Properties[$rootKey]
        if ($null -eq $mapProp -or $null -eq $mapProp.Value -or $null -eq $mapProp.Value.PSObject.Properties[$ServerName]) {
            $results.Add([PSCustomObject]@{ Target = $name; Status = 'Unchanged (entry absent)'; Path = $cfgPath })
            continue
        }
        if ($DryRun) {
            $results.Add([PSCustomObject]@{ Target = $name; Status = 'WouldRemove'; Path = $cfgPath })
            continue
        }
        [void]$mapProp.Value.PSObject.Properties.Remove($ServerName)
        $json = ($rootObj | ConvertTo-Json -Depth 20) + [Environment]::NewLine
        Write-Utf8NoBom -Path $cfgPath -Content $json
        $results.Add([PSCustomObject]@{ Target = $name; Status = 'Removed'; Path = $cfgPath })
        continue
    }

    $detected = $exists -or (Test-Path -LiteralPath $info.DetectDir -PathType Container)
    if (!$explicitSelection -and !$CreateMissing -and !$detected) {
        $results.Add([PSCustomObject]@{ Target = $name; Status = 'Skipped (client not detected)'; Path = $cfgPath })
        continue
    }

    $rootObj = Read-JsonObject -Path $cfgPath
    if ($null -eq $rootObj.PSObject.Properties[$rootKey] -or $null -eq $rootObj.$rootKey) {
        $rootObj | Add-Member -MemberType NoteProperty -Name $rootKey -Value (New-Object PSCustomObject) -Force
    }

    $serversObj = $rootObj.$rootKey
    if ($serversObj -isnot [PSCustomObject]) {
        throw "Config file '$cfgPath' property '$rootKey' is not a JSON object."
    }

    $entry = New-Object PSCustomObject
    if ($info.UseStdio) {
        $entry | Add-Member -MemberType NoteProperty -Name 'type' -Value 'stdio'
    }
    $entry | Add-Member -MemberType NoteProperty -Name 'command' -Value $exePath

    $existingEntry = $serversObj.PSObject.Properties[$ServerName]
    $alreadyMatches = $false
    if ($null -ne $existingEntry -and $null -ne $existingEntry.Value) {
        $curCmd = $existingEntry.Value.PSObject.Properties['command']
        if ($null -ne $curCmd -and $curCmd.Value -eq $exePath) {
            if (!$info.UseStdio -or ($null -ne $existingEntry.Value.PSObject.Properties['type'] -and $existingEntry.Value.type -eq 'stdio')) {
                $alreadyMatches = $true
            }
        }
    }

    if ($alreadyMatches) {
        $results.Add([PSCustomObject]@{ Target = $name; Status = 'Unchanged'; Path = $cfgPath })
        continue
    }

    if ($DryRun) {
        $results.Add([PSCustomObject]@{ Target = $name; Status = 'WouldUpdate'; Path = $cfgPath })
        continue
    }

    $serversObj | Add-Member -MemberType NoteProperty -Name $ServerName -Value $entry -Force
    $json = ($rootObj | ConvertTo-Json -Depth 20) + [Environment]::NewLine
    Write-Utf8NoBom -Path $cfgPath -Content $json
    $results.Add([PSCustomObject]@{ Target = $name; Status = 'Updated'; Path = $cfgPath })
}

$results | Format-Table -AutoSize
