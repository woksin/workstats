# Install and update

How to install `workstats`, keep it current, and which platforms are supported. Back to the [README](../README.md).

## Install

### Prebuilt binaries

No Rust toolchain is required. Every release includes a `SHA256SUMS` file.

<table>
<tr><td width="145"><b>Homebrew</b><br><sub>macOS · Linux</sub></td><td>

```bash
brew install woksin/workstats/workstats
```

The fully-qualified name taps `woksin/workstats` and trusts that one formula as
it installs it. Homebrew 5.1.15 and newer refuse to load a formula from a
non-official tap until it is trusted, so installing by the short name instead
takes `brew tap woksin/workstats && brew trust woksin/workstats` first, which
trusts the whole tap rather than a single formula.

</td></tr>
<tr><td><b>macOS</b></td><td>

```bash
# Apple silicon
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-macos-arm64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# Intel: replace arm64 with x86_64
```

</td></tr>
<tr><td><b>Linux</b></td><td>

```bash
# x86_64
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-linux-x86_64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# ARM64: replace x86_64 with arm64
```

</td></tr>
<tr><td><b>Windows</b></td><td>

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\workstats\bin" | Out-Null
Invoke-WebRequest https://github.com/woksin/workstats/releases/latest/download/workstats-windows-x86_64.exe `
  -OutFile "$env:LOCALAPPDATA\workstats\bin\workstats.exe"
```

Add that directory to your user `PATH`. A 32-bit
`workstats-windows-x86.exe` is also published.

</td></tr>
</table>

> [!NOTE]
> The macOS binaries are currently unsigned. Downloads made by a browser may
> need `xattr -d com.apple.quarantine workstats`; downloads piped through
> `curl` normally do not receive the quarantine attribute.

### Build from source

```bash
cargo install --git https://github.com/woksin/workstats --locked
```

Or clone the repository and use the platform installer. These compile the
locked release build and preserve existing commands unless `--force` /
`-Force` is explicit.

```bash
# macOS / Linux
./install.sh

# Windows PowerShell
./install.ps1
```

## Updating

`workstats` never phones home on a normal run. Checking for or installing a
new version only ever happens when you ask for it:

```bash
workstats update            # check, download, verify, and install a newer release
workstats update --check    # only report whether a newer version exists
```

`workstats update` fetches the latest release from GitHub, verifies the
downloaded binary against the release's published `SHA256SUMS`, and replaces
the running executable in place. It refuses to run if no prebuilt binary is
published for your platform.

If you'd like a passive reminder instead of running the command yourself, opt
into a throttled (at most once every 24 hours) background check that prints a
one-line footer on normal runs when a newer version is known:

```bash
workstats --check-updates                  # opt in for this run
WORKSTATS_CHECK_UPDATES=1 workstats         # opt in via environment
```

```json
{"check_updates": true}
```

in the [config file](configuration.md#inputs-and-index) opts in permanently. `--no-update-check`
or `WORKSTATS_NO_UPDATE_CHECK=1` suppresses both the background check and the
footer for a single run, regardless of how it was enabled. Every check is a
plain HTTPS request to GitHub's public release API—no other data leaves the
machine, and nothing is ever installed without `workstats update`.

## Platforms

CI builds and tests every change on **macOS, Linux, and Windows**. Releases ship:

- macOS: Apple silicon and Intel;
- Linux: x86_64 and ARM64;
- Windows: x86_64 and x86.

Rust 1.88 or newer is supported. Git is discovered from standard locations and
`PATH`; set `WORKSTATS_GIT` to an absolute executable path when needed.
