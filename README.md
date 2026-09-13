# Comrade

An agentic coding cockpit: a Rust TUI (plus a headless runner) that drives an LLM
agent through real work in your repository — files, git, tests, memory and MCP
tools — with a plan you can follow while it runs.

## Install

Prebuilt binaries are published on the [releases page](../../releases). Each
command below resolves the latest release tag and installs the `comrade` binary
into `/usr/local/bin` (drop the `sudo` if that directory is writable for you).

### Linux (x86_64)

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-linux-x86_64.tar.gz" | sudo tar -xz -C /usr/local/bin comrade
```

### macOS (Apple silicon)

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-macos-arm64.tar.gz" | sudo tar -xz -C /usr/local/bin comrade
```

### Windows

With Git Bash:

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/kmruiz/comrade/releases/latest)")
curl -fsSL "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-windows-x86_64.zip" -o comrade.zip
unzip -o comrade.zip -d "$LOCALAPPDATA/Programs/comrade"
```

Or with PowerShell:

```powershell
$tag = (Invoke-WebRequest -UseBasicParsing https://github.com/kmruiz/comrade/releases/latest).BaseResponse.RequestMessage.RequestUri.Segments[-1]
$dir = "$env:LOCALAPPDATA\Programs\comrade"; New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest "https://github.com/kmruiz/comrade/releases/download/$tag/comrade-$tag-windows-x86_64.zip" -OutFile comrade.zip
Expand-Archive comrade.zip -DestinationPath $dir -Force
```

> Private repository? The unauthenticated `curl` calls above 404. Authenticate
> first — e.g. `export GH_TOKEN=$(gh auth token)` and add
> `-H "Authorization: token $GH_TOKEN"` to the `curl` calls, or use
> `gh release download kmruiz/comrade --pattern 'comrade-*-linux-x86_64.tar.gz'`.

## Build from source

```bash
cargo build --release --bin comrade   # target/release/comrade
```

## Releasing

Releases are cut by pushing a `vX.Y.Z` tag, which triggers the release workflow
(builds the binary on Linux/macOS/Windows and creates the GitHub release with
notes generated from the commits since the previous release):

```bash
./release.sh patch   # v0.1.0 -> v0.1.1
./release.sh minor   # v0.1.0 -> v0.2.0
./release.sh major   # v0.1.0 -> v1.0.0
```
