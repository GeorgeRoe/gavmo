// Loads the playwright-core that the browser server (@playwright/mcp) installed -
// it has the `page.screencast` API - and launches the Nix-built Chromium headless.
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { execSync } from 'node:child_process';

const DEFAULT = '/home/agent/.npm-global/lib/node_modules/@playwright/mcp/node_modules/playwright-core/package.json';

function findPlaywrightCore() {
  if (fs.existsSync(DEFAULT)) return DEFAULT;
  const found = execSync(
    "find /home/agent/.npm-global -path '*playwright-core/package.json' 2>/dev/null || true",
    { encoding: 'utf8' },
  ).trim().split('\n').filter(Boolean);
  if (!found.length) {
    throw new Error('playwright-core not found under /home/agent/.npm-global. Is the browser server (@playwright/mcp) installed? See README "Troubleshooting".');
  }
  return found[0];
}

export function loadPlaywright() {
  return createRequire(findPlaywrightCore())('playwright-core');
}

export async function launch({ width, height, storageState, locale = 'en-GB' }) {
  const { chromium } = loadPlaywright();
  const executablePath = process.env.PLAYWRIGHT_MCP_EXECUTABLE_PATH;
  if (!executablePath) throw new Error('PLAYWRIGHT_MCP_EXECUTABLE_PATH is not set (it comes from gavmo.nix).');
  const browser = await chromium.launch({
    executablePath,
    headless: true,
    args: ['--no-sandbox', '--hide-scrollbars'],
  });
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 1,
    locale,
    ...(storageState ? { storageState } : {}),
  });
  const page = await context.newPage();
  if (typeof page.screencast?.start !== 'function') {
    await browser.close();
    throw new Error('This playwright-core has no page.screencast API (needs 1.64+, which @playwright/mcp 0.0.83 ships).');
  }
  return { browser, context, page };
}
