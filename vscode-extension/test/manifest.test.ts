import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

interface ToolContribution {
  name: string;
  toolReferenceName?: string;
  modelDescription?: string;
  canBeReferencedInPrompt?: boolean;
  inputSchema?: unknown;
}

void test('manifest metadata, branding PNGs, walkthrough, and 7 native tools are valid', async () => {
  const packagePath = resolve(__dirname, '../../package.json');
  const manifest = JSON.parse(await readFile(packagePath, 'utf8')) as {
    publisher: string;
    name: string;
    displayName: string;
    description: string;
    icon: string;
    extensionKind?: string[];
    activationEvents?: string[];
    capabilities?: {
      agentsWindow?: { supported?: boolean };
      untrustedWorkspaces?: { supported?: boolean };
    };
    contributes?: {
      languageModelTools?: ToolContribution[];
      chatInstructions?: Array<{ path?: string }>;
      walkthroughs?: Array<{
        id: string;
        steps: Array<{ id: string; completionEvents: string[] }>;
      }>;
    };
  };

  assert.equal(manifest.publisher, 'moonchoi');
  assert.equal(manifest.name, 'test-context-native');
  assert.equal(manifest.displayName, 'Test Context');
  assert.ok(manifest.description.length <= 140);
  assert.deepEqual(manifest.extensionKind, ['workspace', 'ui']);
  assert.equal(manifest.capabilities?.agentsWindow?.supported, true);
  assert.equal(manifest.capabilities?.untrustedWorkspaces?.supported, false);

  for (const [name, width, height] of [
    ['icon', 256, 256],
    ['banner', 1440, 480],
  ] as const) {
    const image = await readFile(resolve(__dirname, `../../resources/${name}.png`));
    assert.equal(image.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
    assert.equal(image.readUInt32BE(16), width);
    assert.equal(image.readUInt32BE(20), height);
  }

  const tools = manifest.contributes?.languageModelTools ?? [];
  assert.deepEqual(
    tools.map((t) => t.name).sort(),
    [
      'test-context-native_testAffected',
      'test-context-native_testGraph',
      'test-context-native_testInvalidate',
      'test-context-native_testRun',
      'test-context-native_testStatus',
      'test-context-native_testSync',
      'test-context-native_testWorkspaces',
    ],
  );
  const statusTool = tools.find((tool) => tool.name === 'test-context-native_testStatus');
  const statusSchema = statusTool?.inputSchema as {
    properties?: { load?: { default?: boolean } };
  };
  assert.equal(statusSchema.properties?.load?.default, false);
  assert.deepEqual(
    tools.map((t) => t.toolReferenceName).sort(),
    [
      'tctxNativeAffected',
      'tctxNativeGraph',
      'tctxNativeInvalidate',
      'tctxNativeRun',
      'tctxNativeStatus',
      'tctxNativeSync',
      'tctxNativeWorkspaces',
    ],
  );
  const instructions = await readFile(
    resolve(__dirname, '../../prompts/test-context.instructions.md'),
    'utf8',
  );
  assert.match(instructions, /MUST use `#tctxNativeRun` FIRST/);
  assert.match(instructions, /Do not choose MCP when the matching Native tool is callable/);

  const walkthrough = manifest.contributes?.walkthroughs?.[0];
  assert.equal(walkthrough?.id, 'getting-started');
  const checkStep = walkthrough?.steps.find((s) => s.id === 'check');
  assert.deepEqual(checkStep?.completionEvents, ['onContext:testContextNative.selfTestPassed']);
});

void test('extension makes the Test Context output channel discoverable once', async () => {
  const source = await readFile(resolve(__dirname, '../../src/extension.ts'), 'utf8');
  const subscriptionsEnd = source.indexOf('\n  );\n\n');
  const discovery = source.indexOf(
    'if (!context.globalState.get<boolean>(OUTPUT_DISCOVERED_STATE_KEY, false))',
  );
  const activationLog = source.indexOf('output.appendLine(`[ready] native endpoint=');
  assert.ok(subscriptionsEnd >= 0);
  assert.ok(discovery > subscriptionsEnd);
  assert.ok(activationLog > discovery);
  assert.equal(source.match(/OUTPUT_DISCOVERED_STATE_KEY/g)?.length, 3);
});
