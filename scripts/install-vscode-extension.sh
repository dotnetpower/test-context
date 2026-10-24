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

if [[ -n "${WSL_DISTRO_NAME:-}" ]] && command -v powershell.exe >/dev/null && command -v wslpath >/dev/null; then
    windows_profile="${TCTX_VSCODE_PROFILE:-FDAI}"
    windows_home="$(
        cd /mnt/c
        powershell.exe -NoProfile -Command '[Environment]::GetFolderPath("UserProfile")'
    )"
    windows_home="${windows_home//$'\r'/}"
    windows_home_wsl="$(wslpath -u "$windows_home")"
    windows_vsix_wsl="$windows_home_wsl/AppData/Local/Temp/test-context-native-$$.vsix"
    trap 'rm -f "${windows_vsix_wsl:-}"' EXIT
    install -m 0600 "$extension/dist/test-context-native.vsix" "$windows_vsix_wsl"
    windows_vsix="$(wslpath -w "$windows_vsix_wsl")"
    # shellcheck disable=SC2016
    (
        cd /mnt/c
        powershell.exe -NoProfile -Command \
            '& { param($Vsix) Set-Location C:\; code --install-extension $Vsix --force }' \
            "$windows_vsix"
    )
    # shellcheck disable=SC2016
    (
        cd /mnt/c
        powershell.exe -NoProfile -Command \
            '& { param($Profile, $Vsix) Set-Location C:\; code --profile $Profile --install-extension $Vsix --force }' \
            "$windows_profile" "$windows_vsix"
    )
    rm -f "$windows_vsix_wsl"
    windows_vsix_wsl=""
    # shellcheck disable=SC2016
    (
        cd /mnt/c
        powershell.exe -NoProfile -Command \
            '& { Set-Location C:\; code --list-extensions --show-versions }'
    ) |
        tr -d '\r' |
        grep '^moonchoi\.test-context-native@'
    # shellcheck disable=SC2016
    (
        cd /mnt/c
        powershell.exe -NoProfile -Command \
            '& { param($Profile) Set-Location C:\; code --profile $Profile --list-extensions --show-versions }' \
            "$windows_profile"
    ) |
        tr -d '\r' |
        grep '^moonchoi\.test-context-native@'

    windows_user_data="$windows_home_wsl/AppData/Roaming/Code/User"
    python3 "$extension/scripts/configure-windows-profile.py" \
        --user-data "$windows_user_data" \
        --distribution "$WSL_DISTRO_NAME" \
        --executable "$(realpath "$HOME/.local/bin/tctx")"
fi

if [[ "${TCTX_INSTALL_MCP:-0}" == "1" ]]; then
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
fi

printf 'Native tools installed as the default. At a safe boundary reload the affected window, then run: Test Context: Getting Started\n'
printf 'Windows Agent Sessions use the WSL native bridge when installed from WSL.\n'
printf 'For an MCP fallback installation, rerun with TCTX_INSTALL_MCP=1.\n'
