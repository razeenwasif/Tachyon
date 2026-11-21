#!/bin/zsh
# Deploy a headless `tachyon` multiplexer server + CLI binary and shell
# integration scripts to a remote SSH host (e.g. Mac mini `kagutsuchi`), and
# optionally install & start its persistent user service (`launchd` / `systemd`).
#
# Usage:
#   scripts/deploy-host.sh kagutsuchi               # build locally & deploy (same arch/OS)
#   scripts/deploy-host.sh kagutsuchi --service     # ...and install launchd/systemd user service
#   scripts/deploy-host.sh forge --remote-build     # sync source and build on remote host (Linux)
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <ssh-host> [--service] [--remote-build]" >&2
    exit 1
fi

host=$1
shift
install_service=0
remote_build=0
for arg in "$@"; do
    case "$arg" in
        --service) install_service=1 ;;
        --remote-build) remote_build=1 ;;
        *) echo "unknown option: $arg" >&2; exit 1 ;;
    esac
done

root=${0:A:h:h}
cd "$root"

if [[ $remote_build -eq 1 ]]; then
    echo "building headless tachyon on $host..."
    ssh "$host" 'mkdir -p ~/.local/bin ~/.local/share/tachyon/shell ~/.cache/tachyon-src'
    rsync -az --delete --exclude target --exclude dist --exclude .git ./ "$host:~/.cache/tachyon-src/"
    ssh "$host" 'export PATH="$HOME/.cargo/bin:$PATH"; cd ~/.cache/tachyon-src && cargo build --release --no-default-features && cp target/release/tachyon ~/.local/bin/tachyon && cp -R shell/* ~/.local/share/tachyon/shell/'
else
    echo "building headless tachyon (arm64 macOS)..."
    cargo build --release --no-default-features
    bin=$(cargo build --release --no-default-features --message-format=json 2>/dev/null \
        | sed -n 's/.*"executable":"\([^"]*\/tachyon\)".*/\1/p' | tail -1)
    [[ -x "$bin" ]] || { echo "release binary not found" >&2; exit 1; }
    codesign --force --sign - "$bin" >/dev/null 2>&1 || true

    echo "deploying to $host:~/.local/bin/tachyon..."
    ssh "$host" 'mkdir -p ~/.local/bin ~/.local/share/tachyon/shell'
    scp -q "$bin" "$host:~/.local/bin/tachyon.new"
    scp -qr shell/* "$host:~/.local/share/tachyon/shell/"
    ssh "$host" 'chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && (codesign --force --sign - ~/.local/bin/tachyon >/dev/null 2>&1 || true)'
fi

if [[ $install_service -eq 1 ]]; then
    echo "installing persistent user service on $host..."
    ssh "$host" '~/.local/bin/tachyon server stop >/dev/null 2>&1 || true; ~/.local/bin/tachyon server install-service'
fi

echo "verifying remote host over SSH bridge..."
local_bin=$(cargo build --release --no-default-features --message-format=json 2>/dev/null \
    | sed -n 's/.*"executable":"\([^"]*\/tachyon\)".*/\1/p' | tail -1)
"$local_bin" hosts add "$host" "ssh://$host" >/dev/null
"$local_bin" hosts test "$host"
