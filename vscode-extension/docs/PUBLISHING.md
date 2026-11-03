# Packaging and publishing

## Build an installable VSIX

From `vscode-extension/`, with Node.js 22 or newer:

```sh
npm ci
npm run check
npm run package
```

The output is `dist/test-context-native.vsix`. VSCE runs lint, tests, and
compilation. Package content is allowlisted: runtime JavaScript, native
instructions, documentation, license, manifest, and rendered PNG artwork.

## Marketplace identity

The Marketplace identity is `dotnetpower.test-context-native` with display name
**Test Context by dotnetpower**.

## GitHub releases

Keep the Rust package, extension manifest, and their lockfiles at the same
version, and add an entry to `CHANGELOG.md`. After committing and pushing to
`main`, push a matching `v<version>` tag.

The `Validate and package` workflow runs backend tests and Clippy, Python
configuration tests, extension checks, and VSIX verification. Successful tag
builds publish a GitHub Release containing `test-context-native.vsix` and its
SHA-256 checksum. Download the VSIX and use **Extensions: Install from VSIX**.
This workflow does not publish to the VS Code Marketplace.
