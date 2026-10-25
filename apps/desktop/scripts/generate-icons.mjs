import { mkdtemp, copyFile, mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

// Generate only the assets this desktop app ships. Keep the master lossless;
// rasterisation and Apple's ICNS size variants are owned by Tauri's generator.
const desktop = fileURLToPath(new URL('../', import.meta.url));
const master = fileURLToPath(new URL('../../../assets/brand/codypendent-icon-master.png', import.meta.url));
const staging = await mkdtemp(join(tmpdir(), 'codypendent-icons-'));
try {
  const cli = join(desktop, 'node_modules/@tauri-apps/cli/tauri.js');
  const result = spawnSync(process.execPath, [cli, 'icon', master, '--output', staging], { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Tauri icon generation failed (${result.status})`);
  const icons = join(desktop, 'src-tauri/icons');
  const publicDir = join(desktop, 'public');
  await mkdir(icons, { recursive: true });
  await mkdir(publicDir, { recursive: true });
  for (const name of ['32x32.png', '128x128.png', '128x128@2x.png', 'icon.png', 'icon.icns']) {
    await copyFile(join(staging, name), join(icons, name));
  }
  await copyFile(join(staging, '128x128.png'), join(publicDir, 'codypendent-mark.png'));
  await copyFile(join(staging, '32x32.png'), join(publicDir, 'codypendent-favicon.png'));
  console.log('Updated desktop icons, sidebar mark and favicon.');
} finally {
  await rm(staging, { recursive: true, force: true });
}
