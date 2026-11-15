// Liam ships its renderer locally. Use fonts from the device so the diagram
// also renders on a disconnected demo network.
import { copyFile, readdir, readFile, writeFile } from "node:fs/promises";

const site = new URL("../public/schema/", import.meta.url);
const assets = new URL("assets/", site);
for (const name of await readdir(assets)) {
  if (!name.endsWith(".css")) continue;
  const file = new URL(name, assets);
  const css = await readFile(file, "utf8");
  const localCss = css
    .replace(/@import\s*["']https:\/\/fonts\.googleapis\.com\/[^"']+["'];/g, "")
    .replace(/--main-font:\s*Inter(?=[;}])/g, "--main-font: Inter, system-ui, sans-serif")
    .replace(/--code-font:\s*IBM Plex Mono(?=[;}])/g, "--code-font: 'IBM Plex Mono', ui-monospace, monospace");
  await writeFile(file, localCss);
}

const index = new URL("index.html", site);
const html = await readFile(index, "utf8");
await writeFile(index, html.replace("<title>Liam ERD</title>", "<title>eKYC database schema · Liam ERD</title>"));
await copyFile(new URL("../node_modules/@liam-hq/cli/LICENSE", import.meta.url), new URL("LICENSE", site));
await copyFile(new URL("structure.sql", import.meta.url), new URL("structure.sql", site));
