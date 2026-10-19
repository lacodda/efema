# efema-server installer for Windows:
#   irm https://raw.githubusercontent.com/lacodda/efema/main/tools/install.ps1 | iex
#
# $env:EFEMA_VERSION pins a release (v0.6.0); $env:EFEMA_INSTALL_DIR is where
# the binary goes (%LOCALAPPDATA%\Programs\efema).
$ErrorActionPreference = "Stop"

$repo = "lacodda/efema"

# The tag comes from the /releases/latest redirect rather than the REST API:
# unauthenticated API calls are capped at 60 an hour per address, and an
# installer that fails because someone else behind the same address ran it is
# no installer.
$tag = $env:EFEMA_VERSION
if (-not $tag) {
    $request = [Net.HttpWebRequest]::Create("https://github.com/$repo/releases/latest")
    $request.AllowAutoRedirect = $false
    $request.UserAgent = "efema-installer"
    try {
        $response = $request.GetResponse()
        $tag = ($response.Headers["Location"] -split "/")[-1]
        $response.Close()
    } catch {
        throw "Cannot tell the latest release of ${repo}: $($_.Exception.Message)"
    }
}
if (-not $tag -or $tag -notmatch '^v\d') {
    throw "Cannot tell the latest release of $repo - set `$env:EFEMA_VERSION to a tag like v0.6.0"
}

$name = "efema-server-$tag-x86_64-pc-windows-msvc"
$url = "https://github.com/$repo/releases/download/$tag/$name.zip"
$dir = if ($env:EFEMA_INSTALL_DIR) { $env:EFEMA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\efema" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) "efema-install-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force $tmp | Out-Null

try {
    Write-Host "Downloading $url"
    Invoke-WebRequest $url -OutFile (Join-Path $tmp "efema-server.zip")
    Expand-Archive (Join-Path $tmp "efema-server.zip") -DestinationPath $tmp -Force
    # Found in what was unpacked, not at a guessed path.
    $bin = Get-ChildItem $tmp -Recurse -File -Filter "efema-server.exe" | Select-Object -First 1
    if (-not $bin) {
        throw "The archive $name.zip holds no efema-server.exe"
    }
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item $bin.FullName $dir -Force
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# The user PATH is edited in the registry, not through the .NET environment
# API: that call rewrites the value as REG_SZ, and a PATH stored as REG_SZ no
# longer expands entries such as %JAVA_HOME%\bin - they turn into literal text
# and silently stop working. Reading the raw value keeps those entries as
# typed; writing it back as ExpandString keeps the type. A PATH that cannot be
# edited is not a failed install: the binary is in place, the user is told
# where.
try {
    $key = Get-Item "HKCU:\Environment"
    $raw = [string]$key.GetValue("Path", "", "DoNotExpandEnvironmentNames")
    $entries = @($raw -split ";" | Where-Object { $_ })
    $wanted = $dir.TrimEnd("\")
    $present = $entries | Where-Object { $_.TrimEnd("\") -ieq $wanted }
    if (-not $present) {
        $value = if ($entries.Count -gt 0) { ($entries + $wanted) -join ";" } else { $wanted }
        Set-ItemProperty -Path "HKCU:\Environment" -Name Path -Value $value -Type ExpandString
        # Tell running shells and Explorer that the environment changed, the
        # way the System Properties dialog does; without it only new logins see
        # the entry.
        if (-not ("EfemaInstall.Env" -as [type])) {
            Add-Type -Namespace EfemaInstall -Name Env -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);
'@
        }
        $result = [System.UIntPtr]::Zero
        # HWND_BROADCAST = 0xffff, WM_SETTINGCHANGE = 0x1A, SMTO_ABORTIFHUNG = 0x2
        [EfemaInstall.Env]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, "Environment", 0x2, 5000, [ref]$result) | Out-Null
        Write-Host "Added $dir to your user PATH - open a new terminal to pick it up."
    }
} catch {
    Write-Host "Note: could not update the user PATH ($($_.Exception.Message)); add $dir to it yourself."
}
$version = & (Join-Path $dir "efema-server.exe") --version
Write-Host "Installed $version to $dir\efema-server.exe"
Write-Host "Next: https://lacodda.github.io/efema/getting-started/"
