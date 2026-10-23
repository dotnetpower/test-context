import { mkdir, rm } from 'node:fs/promises';

await Promise.all([
  rm(new URL('../out', import.meta.url), { recursive: true, force: true }),
  rm(new URL('../dist', import.meta.url), { recursive: true, force: true }),
]);
await mkdir(new URL('../dist', import.meta.url), { recursive: true });
