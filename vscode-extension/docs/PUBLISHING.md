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
