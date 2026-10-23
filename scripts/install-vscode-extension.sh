#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
extension="$root/vscode-extension"

command -v node >/dev/null
command -v npm >/dev/null
command -v code >/dev/null

cd "$extension"
npm ci
npm run package

code --install-extension "$extension/dist/test-context-native.vsix" --force
code --list-extensions --show-versions | grep '^moonchoi\.test-context-native@'

printf 'At a safe boundary reload the affected window, then run: Test Context: Getting Started\n'
