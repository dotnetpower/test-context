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

mcp_file="$HOME/.vscode-server/data/User/mcp.json"
if [[ -f "$mcp_file" ]]; then
    node - "$mcp_file" "$HOME/.local/bin/tctx" <<'NODE'
const fs = require('node:fs');

const [path, command] = process.argv.slice(2);
const source = fs.readFileSync(path, 'utf8');
const document = JSON.parse(source);
document.servers ??= {};
document.servers['test-context'] = {
  type: 'stdio',
  command,
  args: ['shared-mcp'],
  cwd: process.env.HOME,
};
const temporary = `${path}.tmp.${process.pid}`;
fs.writeFileSync(temporary, `${JSON.stringify(document, null, 2)}\n`, { mode: 0o600 });
fs.renameSync(temporary, path);
NODE
    node -e '
const fs = require("node:fs");
const document = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
const server = document.servers?.["test-context"];
if (server?.command !== process.argv[2] || server?.args?.join("\0") !== "shared-mcp") {
  process.exit(1);
}
' "$mcp_file" "$HOME/.local/bin/tctx"
else
    code --add-mcp "{\"name\":\"test-context\",\"type\":\"stdio\",\"command\":\"$HOME/.local/bin/tctx\",\"args\":[\"shared-mcp\"],\"cwd\":\"$HOME\"}"
fi

printf 'At a safe boundary reload the affected window, then run: Test Context: Getting Started\n'
