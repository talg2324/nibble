# nibble installer for Windows (PowerShell 5.1 or 7). On macOS, Linux and inside WSL, use install.sh.
#
#   irm https://github.com/talg2324/nibble/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://github.com/talg2324/nibble/releases/latest/download/install.ps1))) -Yes -Remotes
#   & ([scriptblock]::Create((irm https://github.com/talg2324/nibble/releases/latest/download/install.ps1))) -Uninstall
#   $env:NIB_VERSION = 'v0.4-rc.1'; irm …/install.ps1 | iex     # that release (also -Version TAG)
#
# "Latest" never means a prerelease (a release candidate, vX.Y-rc.N): those only by name.
# Installs the latest release into %LOCALAPPDATA%\Programs\nib and puts that folder on your user
# PATH. Running it again updates in place. It runs inside your PowerShell session (iex), so it
# never calls `exit`.
param(
    [switch]$Yes,
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\nib'),
    [switch]$Remotes,
    [switch]$NoRemotes,
    [switch]$NoModifyPath,
    [switch]$Upstream,
    [switch]$NoUpstream,
    [switch]$Uninstall,
    [string]$Version = $env:NIB_VERSION
)

function Install-Nib {
    param($Yes, $Prefix, $Remotes, $NoRemotes, $NoModifyPath, $Upstream, $NoUpstream, $Uninstall, $Version)

    $Repo = 'talg2324/nibble'
    $Triple = 'x86_64-pc-windows-msvc'
    $DaemonTriples = @('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl')
    $Dir = [IO.Path]::GetFullPath($Prefix)
    $State = Join-Path $Dir 'install-state'
    $Shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\nib.lnk'
    $ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest's progress bar slows downloads a lot
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # ── look: nib's default theme ──────────────────────────────────────────
    $E = [char]27
    $Plain = [Console]::IsOutputRedirected
    function rgb($hex) { if ($Plain) { return '' }; "$E[38;2;$([Convert]::ToInt32($hex.Substring(0,2),16));$([Convert]::ToInt32($hex.Substring(2,2),16));$([Convert]::ToInt32($hex.Substring(4,2),16))m" }
    $WHITE = rgb 'ffffff'; $FG = rgb 'd4d4d4'; $D = rgb '6e7681'; $A = rgb '3b8eea'; $LOGO = rgb '007acc'
    $G = rgb '3fb950'; $Y = rgb 'e5c07b'; $RED = rgb 'f85149'
    if ($Plain) { $SEL = ''; $B = ''; $R = ''; $EL = '' } else { $SEL = "$E[48;2;38;79;120m"; $B = "$E[1m"; $R = "$E[0m$FG"; $EL = "$E[K" }
    $Interactive = -not $Yes -and -not [Console]::IsInputRedirected -and -not $Plain
    $Notes = New-Object System.Collections.Generic.List[string]

    function say($s) { Write-Host "$s" }
    function rule { say "  $D─────────────────────────────────────────────────────────────$R" }
    function tilde($p) { if ($p.StartsWith($HOME)) { '~' + $p.Substring($HOME.Length) } else { $p } }
    function fit($h, $w) {
        $w = [Math]::Max(10, $w)
        if ($h.Length -gt $w) { $h.Substring(0, $w - 1) + '…' } else { $h }
    }
    function Cols { try { [Console]::WindowWidth } catch { 100 } }

    function Banner {
        say ''
        say "    $WHITE▄████▄ ▄████▄ ▄████▄ $LOGO▄██▄$R"
        say "    $WHITE██████ ██████ ██████ $LOGO█████▄$R"
        say "    $WHITE▀████▀ ▀████▀ ▀████▀ $LOGO▀████▀$R"
        say "               $B${WHITE}nibble$R"
        say ''
    }

    # ── downloads ──────────────────────────────────────────────────────────
    function Fetch($url, $out) { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out }
    function LatestTag {
        # releases/latest redirects to …/releases/tag/vX.Y (no API call, no rate limit)
        $req = [Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
        $req.AllowAutoRedirect = $false
        $req.Method = 'HEAD'
        $res = $req.GetResponse()
        try { $to = $res.Headers['Location'] } finally { $res.Close() }
        if ($to -match '/tag/([^/]+)$') { return $Matches[1] }
        throw "no release found at https://github.com/$Repo/releases"
    }
    function Verify($file, $sums) {
        $name = Split-Path $file -Leaf
        $want = $null
        foreach ($line in Get-Content $sums) {
            $p = $line -split '\s+', 2
            if ($p.Count -eq 2 -and $p[1].TrimStart('*') -eq $name) { $want = $p[0] }
        }
        if (-not $want) { throw "$name is not in SHA256SUMS" }
        if ((Get-FileHash -Algorithm SHA256 $file).Hash -ne $want.ToUpper()) { throw "${name}: checksum does not match" }
    }
    # A folder's contents as one hash: tells whether the user edited a skill we installed.
    function DirHash($dir) {
        $root = (Resolve-Path $dir).Path
        $lines = Get-ChildItem -Recurse -File $root | Sort-Object FullName | ForEach-Object {
            $_.FullName.Substring($root.Length).Replace('\', '/') + ' ' + (Get-FileHash -Algorithm SHA256 $_.FullName).Hash
        }
        $bytes = [Text.Encoding]::UTF8.GetBytes(($lines -join "`n"))
        $sha = [Security.Cryptography.SHA256]::Create()
        -join ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') })
    }
    function ReadState {
        $s = @{}
        if (Test-Path $State) { foreach ($l in Get-Content $State) { $p = $l -split ' ', 2; if ($p.Count -eq 2) { $s[$p[0]] = $p[1] } } }
        $s
    }

    # ── user PATH, kept as REG_EXPAND_SZ so %VARS% in it survive ───────────
    function UserPath {
        (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    }
    function SetUserPath($value) {
        Set-ItemProperty 'HKCU:\Environment' -Name Path -Value $value -Type ExpandString
        # tell running programs (Explorer, new terminals) that the environment changed
        [Environment]::SetEnvironmentVariable('NIB_INSTALLER_PING', '1', 'User')
        [Environment]::SetEnvironmentVariable('NIB_INSTALLER_PING', $null, 'User')
    }
    function OnPath { (UserPath) -split ';' | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') } }

    # ── steps ──────────────────────────────────────────────────────────────
    function Step($title, $what, [scriptblock]$do) {
        $what = fit $what ((Cols) - $title.Length - 8)
        if (-not $Plain) { Write-Host -NoNewline "  $A…$R $title  $D$what$R" }
        try { & $do } catch {
            if (-not $Plain) { Write-Host -NoNewline "`r$EL" }
            say "  $RED✗$R $title  $D$what$R"
            say "      $($_.Exception.Message)"
            throw 'stopped'
        }
        if (-not $Plain) { Write-Host -NoNewline "`r$EL" }
        say "  $G✓$R $title  $D$what$R"
    }

    function Running {
        Get-Process nib, nibd -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase) }
    }

    # ── uninstall ──────────────────────────────────────────────────────────
    function Remove-Nib {
        say "  $B${WHITE}Remove nib$R  ${D}your settings and your projects stay$R"
        say ''
        if (-not (Test-Path $Dir)) { say "  ${D}nib isn't installed in $(tilde $Dir).$R"; return }
        if (Running) { say "  $Y!$R nib is running from $(tilde $Dir). Close it, then run this again."; return }
        if ($Interactive) {
            $ans = Read-Host "  Remove it? [y/N]"
            if ($ans -notmatch '^(y|yes)$') { say "  ${D}nothing was changed.$R"; return }
        }
        rule
        Step 'Agent skills' "the ones it installed and you didn't edit" {
            $s = ReadState
            foreach ($p in $s.Keys) {
                if (-not (Test-Path $p)) { continue }
                if ((DirHash $p) -eq $s[$p]) { Remove-Item -Recurse -Force $p } else { $Notes.Add("$Y!$R kept your edited $(tilde $p)") }
            }
        }
        Step 'PATH' 'your user PATH' {
            $kept = (UserPath) -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ine $Dir.TrimEnd('\') }
            SetUserPath ($kept -join ';')
        }
        Step 'Start menu' 'nib.lnk' { Remove-Item -Force $Shortcut -ErrorAction SilentlyContinue }
        Step 'Program' (tilde $Dir) { Remove-Item -Recurse -Force $Dir }
        rule
        if ($Notes.Count) { say ''; $Notes | ForEach-Object { say "  $_" } }
        say ''; say "  $G${B}nib is removed.$R"; say ''
    }

    # ── install ────────────────────────────────────────────────────────────
    Write-Host -NoNewline $FG
    Banner
    if ($Uninstall) { Remove-Nib; return }

    $arch = $env:PROCESSOR_ARCHITECTURE
    $os = (Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue).Caption
    if (-not $os) { $os = 'Windows' }
    if ($Version) {
        $Tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
    } else {
        try { $Tag = LatestTag } catch { say "  $RED✗$R Couldn't reach GitHub to find the latest release (https://github.com/$Repo/releases)."; return }
    }
    $have = $null
    if (Test-Path (Join-Path $Dir 'nib.exe')) { $have = (& (Join-Path $Dir 'nib.exe') --version 2>$null | Select-Object -First 1) }
    $sshHosts = 0
    $sshConfig = Join-Path $HOME '.ssh\config'
    if (Test-Path $sshConfig) { $sshHosts = @(Select-String -Path $sshConfig -Pattern '^\s*host\s+[^*?]+$').Count }
    $distros = 0
    if (Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss') { $distros = @(Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss').Count }
    $harn = @('claude', 'codex', 'gemini', 'opencode', 'ollama') | Where-Object { Get-Command $_ -ErrorAction SilentlyContinue }
    $hasGit = [bool](Get-Command git -ErrorAction SilentlyContinue)
    $onPath = [bool](OnPath)

    function row($k, $v) { say ("  $D{0,-9}$R {1}" -f $k, $v) }
    row machine "$os · $arch"
    row release $Tag
    row into (tilde $Dir)
    if ($have) { row found "$have  $D— replaced in place, your settings stay$R" }
    if ($harn) { row agents ($harn -join ' ') } else { row agents "${Y}none found$R  ${D}nib works without; its agents tab needs one$R" }
    if (-not $hasGit) { row git "${Y}not found$R  ${D}diffs and agent branches need it$R" }
    if ($arch -eq 'ARM64') { say ''; say "  $A`ℹ$R  This is the x64 build; Windows on ARM runs it emulated." }
    say ''

    # defaults
    $opt = @{}
    $opt.remotes = if ($Remotes) { $true } elseif ($NoRemotes) { $false } else { (Test-Path (Join-Path $Dir 'daemons')) -or $distros -gt 0 -or $sshHosts -gt 0 }
    $opt.path = -not $NoModifyPath
    $opt.upstream = if ($Upstream) { $true } elseif ($NoUpstream) { $false } else { (Test-Path $State) -and [bool](Select-String -Path $State -Pattern '[\\/]nib-upstream ' -Quiet) }

    $wslNote = @()
    if ($distros -gt 0) { $wslNote += "$distros WSL distro$(if ($distros -ne 1) { 's' })" }
    if ($sshHosts -gt 0) { $wslNote += "$sshHosts hosts in ~\.ssh\config" }
    $items = @(
        @{ label = 'nib + nibd'; hint = 'the editor, its daemon, the Start menu entry and agent skills'; key = $null },
        @{ label = 'WSL and SSH'; hint = "projects in WSL or on Linux servers: the Linux daemon (for WSL nib starts it inside WSL; no SSH involved)$(if ($wslNote) { '  ·  ' + ($wslNote -join ', ') })"; key = 'remotes' },
        @{ label = 'Add to PATH'; hint = $(if ($onPath) { "$(tilde $Dir) is on PATH already; nothing to change" } else { 'your user PATH, so `nib` works in any terminal' }); key = 'path' },
        @{ label = 'nib-upstream'; hint = "agents may draft issues / PRs to $Repo, sent only with your OK"; key = 'upstream' }
    )
    function Draw($cur) {
        $w = (Cols) - 28
        for ($i = 0; $i -lt $items.Count; $i++) {
            $it = $items[$i]
            if (-not $it.key) { $box = "$D[■]" } elseif ($opt[$it.key]) { $box = "$A[✓]" } else { $box = "$D[ ]" }
            $h = fit $it.hint $w
            if ($i -eq $cur) { say ("$SEL $A▸ $box $WHITE{0,-17}$FG$h$EL$E[0m$R" -f $it.label) }
            else { say ("   $box $FG{0,-17}$D$h$R$EL" -f $it.label) }
        }
        say "$EL"
        # The keys only when they can be pressed (-1: -Yes, nothing to choose).
        if ($cur -ge 0) { say "  $A↑↓$D move  ${A}space$D toggle  ${A}enter$D install  ${A}q$D quit$R$EL" }
    }

    if ($Interactive) {
        say "  $B${WHITE}What to install$R"
        say ''
        $cur = 1
        [Console]::CursorVisible = $false
        try {
            Draw $cur
            while ($true) {
                $k = [Console]::ReadKey($true)
                if ($k.Key -eq 'UpArrow' -or $k.KeyChar -eq 'k') { $cur = ($cur - 1 + $items.Count) % $items.Count }
                elseif ($k.Key -eq 'DownArrow' -or $k.KeyChar -eq 'j') { $cur = ($cur + 1) % $items.Count }
                elseif ($k.Key -eq 'Spacebar' -or $k.KeyChar -eq 'x') { $key = $items[$cur].key; if ($key) { $opt[$key] = -not $opt[$key] } }
                elseif ($k.Key -eq 'Enter') { break }
                elseif ($k.KeyChar -eq 'q') { say ''; say "  ${D}nothing was changed.$R"; return }
                Write-Host -NoNewline "$E[$($items.Count + 2)A"
                Draw $cur
            }
        } finally { [Console]::CursorVisible = $true }
        say ''
    } else {
        say "  $D-Yes: installing$R"
        Draw -1
    }

    if (Running) { say "  $Y!$R nib is running from $(tilde $Dir). Close it, then run this again."; return }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("nib-install-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory $tmp | Out-Null
    $new = "$Dir.new"
    try {
        $rel = "https://github.com/$Repo/releases/download/$Tag"
        $zip = Join-Path $tmp "nibble-$Tag-$Triple.zip"
        rule
        Step 'Download' "nibble-$Tag-$Triple.zip$(if ($opt.remotes) { ' + the Linux daemons' })" {
            Fetch "$rel/SHA256SUMS" (Join-Path $tmp 'SHA256SUMS')
            Fetch "$rel/nibble-$Tag-$Triple.zip" $zip
            if ($opt.remotes) { foreach ($t in $DaemonTriples) { Fetch "$rel/nibd-$Tag-$t.tar.gz" (Join-Path $tmp "nibd-$Tag-$t.tar.gz") } }
        }
        Step 'Check sha256' "against the release's SHA256SUMS" {
            Get-ChildItem $tmp -File | Where-Object { $_.Name -ne 'SHA256SUMS' } | ForEach-Object { Verify $_.FullName (Join-Path $tmp 'SHA256SUMS') }
        }
        Step 'Unpack' (tilde $Dir) {
            # Unpacked beside the installed one, then swapped in: a failed step leaves it as it was.
            if (Test-Path $new) { Remove-Item -Recurse -Force $new }
            $x = Join-Path $tmp 'x'
            Expand-Archive -Path $zip -DestinationPath $x
            # Its parent (%LOCALAPPDATA%\Programs) is not there on a fresh Windows.
            New-Item -ItemType Directory -Force (Split-Path $new) | Out-Null
            Move-Item (Join-Path $x "nibble-$Tag-$Triple") $new
            if ($opt.remotes) {
                foreach ($t in $DaemonTriples) {
                    & tar.exe -xzf (Join-Path $tmp "nibd-$Tag-$t.tar.gz") -C $x
                    if ($LASTEXITCODE -ne 0) { throw "couldn't unpack nibd-$Tag-$t.tar.gz" }
                    New-Item -ItemType Directory -Force (Join-Path $new "daemons\$t") | Out-Null
                    Move-Item (Join-Path $x "nibd-$Tag-$t\nibd") (Join-Path $new "daemons\$t\nibd")
                }
            }
            if (Test-Path $State) { Copy-Item $State (Join-Path $new 'install-state') }
            Get-ChildItem -Recurse $new | Unblock-File
        }
        Step 'Swap in' $(if ($have) { "replacing $have" } else { tilde $Dir }) {
            $old = "$Dir.old"
            if (Test-Path $old) { Remove-Item -Recurse -Force $old }
            if (Test-Path $Dir) { Move-Item $Dir $old }
            Move-Item $new $Dir
            if (Test-Path $old) { Remove-Item -Recurse -Force $old }
        }
        if ($opt.path -and -not $onPath) {
            Step 'Add to PATH' 'your user PATH' {
                $p = UserPath
                SetUserPath ($(if ($p) { $p.TrimEnd(';') + ';' } else { '' }) + $Dir)
                $env:Path = $env:Path.TrimEnd(';') + ';' + $Dir
            }
        }
        Step 'Agent skills' "into ~\.claude\skills, ~\.agents\skills$(if ($opt.upstream) { ', with nib-upstream' })" {
            # One the user edited is kept, and they're told where the new one is.
            $src = Join-Path $Dir '.skills'
            $had = ReadState
            $now = New-Object System.Collections.Generic.List[string]
            foreach ($dest in @((Join-Path $HOME '.claude\skills'), (Join-Path $HOME '.agents\skills'))) {
                New-Item -ItemType Directory -Force $dest | Out-Null
                foreach ($s in Get-ChildItem -Directory $src) {
                    $to = Join-Path $dest $s.Name
                    $new = DirHash $s.FullName
                    $was = $had[$to]
                    if ($s.Name -eq 'nib-upstream' -and -not $opt.upstream) {
                        # unchecked: take back one we put there, unless it was edited
                        if ($was -and (Test-Path $to) -and (DirHash $to) -eq $was) { Remove-Item -Recurse -Force $to }
                        continue
                    }
                    if (Test-Path $to) {
                        $cur = DirHash $to
                        if ($cur -ne $new -and $cur -ne $was) {
                            $Notes.Add("$Y!$R kept your edited $(tilde $to); the new one is in $(tilde $s.FullName)")
                            if ($was) { $now.Add("$to $was") }
                            continue
                        }
                        Remove-Item -Recurse -Force $to
                    }
                    Copy-Item -Recurse $s.FullName $to
                    $now.Add("$to $new")
                }
            }
            Set-Content -Path $State -Value $now -Encoding UTF8
        }
        Step 'Start menu' 'nib' {
            $sh = (New-Object -ComObject WScript.Shell).CreateShortcut($Shortcut)
            $sh.TargetPath = Join-Path $Dir 'nib.exe'
            $sh.WorkingDirectory = $HOME
            $sh.Description = 'nibble, an editor for working with agents'
            $sh.Save()
        }
        Step 'Check' 'nib --version' {
            & (Join-Path $Dir 'nib.exe') --version | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "nib.exe --version failed ($LASTEXITCODE)" }
        }
        rule
    } catch {
        if ("$_" -ne 'stopped') { say "  $RED✗$R $_" }
        say ''
        say "  ${RED}Stopped.$R $(if ($have) { 'Your installed nib is as it was.' } else { 'Nothing was installed.' })"
        return
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
        if (Test-Path $new) { Remove-Item -Recurse -Force $new -ErrorAction SilentlyContinue }
    }

    if ($Notes.Count) { say ''; $Notes | ForEach-Object { say "  $_" } }
    say ''
    say "  $G${B}nib $Tag is installed.$R"
    say ''
    if ($opt.path -and -not $onPath) { say "  Open a new terminal, then  ${A}nib .$R" }
    elseif (-not $onPath) { say "  Not on PATH; run it as      $A$(tilde $Dir)\nib.exe$R" }
    else { say "  Open a project              ${A}nib .$R" }
    if ($opt.remotes) { say "  Open one in WSL or on SSH   ${A}nib @myserver$R  ${D}nibd goes up by itself$R" }
    say "  Update                      ${D}run this installer again$R"
    say "  Remove                      $A& ([scriptblock]::Create((irm https://github.com/$Repo/releases/latest/download/install.ps1))) -Uninstall$R"
    say ''
}

$enc = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.Encoding]::UTF8
    Install-Nib -Yes $Yes -Prefix $Prefix -Remotes $Remotes -NoRemotes $NoRemotes -NoModifyPath $NoModifyPath -Upstream $Upstream -NoUpstream $NoUpstream -Uninstall $Uninstall -Version $Version
} finally {
    [Console]::OutputEncoding = $enc
    Write-Host -NoNewline "$([char]27)[0m"
}
