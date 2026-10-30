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

mcp_options=(--executable "$HOME/.local/bin/tctx")
if [[ "${TCTX_INSTALL_MCP:-0}" == "1" ]]; then
    mcp_options+=(--enable)
fi

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

    python3 "$extension/scripts/configure-mcp.py" \
        --file "$windows_user_data/mcp.json" \
        --distribution "$WSL_DISTRO_NAME" "${mcp_options[@]}"
    for profile_mcp in "$windows_user_data"/profiles/*/mcp.json; do
        [[ -f "$profile_mcp" ]] || continue
        python3 "$extension/scripts/configure-mcp.py" \
            --file "$profile_mcp" \
            --distribution "$WSL_DISTRO_NAME" "${mcp_options[@]}"
    done
fi

if [[ -n "${WSL_DISTRO_NAME:-}" ]]; then
    mcp_file="$HOME/.vscode-server/data/User/mcp.json"
else
    mcp_file="${XDG_CONFIG_HOME:-$HOME/.config}/Code/User/mcp.json"
fi
python3 "$extension/scripts/configure-mcp.py" \
    --file "$mcp_file" "${mcp_options[@]}"

printf 'Native tools installed as the default. At a safe boundary reload the affected window, then run: Test Context: Getting Started\n'
printf 'Windows Agent Sessions use the WSL native bridge when installed from WSL.\n'
printf 'For an MCP fallback installation, rerun with TCTX_INSTALL_MCP=1.\n'
