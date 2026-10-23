#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 0755 target/release/tctx "$HOME/.local/bin/tctx.next"
mv "$HOME/.local/bin/tctx.next" "$HOME/.local/bin/tctx"
ln -sf "$HOME/.local/bin/tctx" "$HOME/.local/bin/tctx-pytest"
install -m 0644 deploy/test-context.service "$HOME/.config/systemd/user/test-context.service"
systemctl --user daemon-reload
systemctl --user enable test-context.service
systemctl --user restart test-context.service
for attempt in $(seq 1 50); do
    if "$HOME/.local/bin/tctx" ping >/dev/null; then
        exit 0
    fi
    sleep 0.1
done
printf 'test-context daemon did not become ready\n' >&2
exit 1
