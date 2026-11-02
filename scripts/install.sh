#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/tctx "$HOME/.local/bin/tctx.next"
mv "$HOME/.local/bin/tctx.next" "$HOME/.local/bin/tctx"
ln -sf "$HOME/.local/bin/tctx" "$HOME/.local/bin/tctx-pytest"
ln -sf "$HOME/.local/bin/tctx" "$HOME/.local/bin/tctx-cargo-test"
ln -sf "$HOME/.local/bin/tctx" "$HOME/.local/bin/tctx-vitest"
ln -sf "$HOME/.local/bin/tctx" "$HOME/.local/bin/tctx-jest"

if [[ "$(uname -s)" == "Darwin" ]]; then
    mkdir -p "$HOME/Library/LaunchAgents"
    plist="$HOME/Library/LaunchAgents/com.dotnetpower.test-context.plist"
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.dotnetpower.test-context</string>
    <key>ProgramArguments</key>
    <array>
        <string>$HOME/.local/bin/tctx</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
EOF
    launchctl unload "$plist" 2>/dev/null || true
    launchctl load "$plist"
else
    mkdir -p "$HOME/.config/systemd/user"
    install -m 0644 deploy/test-context.service "$HOME/.config/systemd/user/test-context.service"
    systemctl --user daemon-reload
    systemctl --user enable test-context.service
    systemctl --user restart test-context.service
fi
for attempt in $(seq 1 50); do
    if "$HOME/.local/bin/tctx" ping >/dev/null; then
        exit 0
    fi
    sleep 0.1
done
printf 'test-context daemon did not become ready\n' >&2
exit 1
