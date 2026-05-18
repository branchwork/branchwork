// Audit §20 bundle-size gate. Asserts the eagerly-loaded entry chunk
// stays under MAX_GZIPPED_BYTES gzipped. Lazy chunks (PtyTerminal,
// future code-split routes) are reported but not gated — the budget
// is about what the user pays before the dashboard becomes interactive.
//
// Run via `pnpm --filter @branchwork/web bundle:check` after `build`.

import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

// 145 KB gzipped is the post-lazy baseline (T10.4); +10% headroom rounds
// to 160_000 bytes (~156 KiB). Bump deliberately when an intentional
// addition lands — never auto-relax.
//
// Bumps:
//   - 2026-05-19: 160_000 → 168_000 (~+8 KB). UncommittedWorkBanner
//     gained pausedFiles list + Resume-anyway + Mark-as-operational
//     diff preview (dirty-tree-check-stop-pausing-on-write-noise
//     task 3.2, commit 1a71763). Real product surface, not bloat.
const MAX_GZIPPED_BYTES = 168_000;

const distAssets = join(import.meta.dirname, "..", "dist", "assets");

interface ChunkSize {
  file: string;
  raw: number;
  gzipped: number;
}

async function measure(): Promise<ChunkSize[]> {
  const entries = await readdir(distAssets);
  const jsFiles = entries.filter((f) => f.endsWith(".js"));
  const sizes: ChunkSize[] = [];
  for (const f of jsFiles) {
    const buf = await readFile(join(distAssets, f));
    sizes.push({ file: f, raw: buf.byteLength, gzipped: gzipSync(buf).byteLength });
  }
  return sizes.sort((a, b) => b.gzipped - a.gzipped);
}

function fmt(bytes: number): string {
  return `${(bytes / 1024).toFixed(1)} KB (${bytes.toLocaleString()} bytes)`;
}

async function main(): Promise<void> {
  let chunks: ChunkSize[];
  try {
    chunks = await measure();
  } catch (err) {
    console.error(
      `[bundle-size] could not read ${distAssets}: ${(err as Error).message}\n` +
        `[bundle-size] run 'pnpm --filter @branchwork/web build' first.`,
    );
    process.exit(2);
  }

  // The Vite entry chunk is the one referenced from index.html and
  // is named `index-*.js` by default. Lazy chunks (e.g. PtyTerminal)
  // are split out as separate files and pulled in on demand.
  const entry = chunks.find((c) => c.file.startsWith("index-") && c.file.endsWith(".js"));
  if (!entry) {
    console.error(
      `[bundle-size] no index-*.js entry chunk found in ${distAssets}.\n` +
        `[bundle-size] chunks present: ${chunks.map((c) => c.file).join(", ") || "(none)"}`,
    );
    process.exit(2);
  }

  const lazy = chunks.filter((c) => c !== entry);

  console.log(`[bundle-size] entry chunk: ${entry.file}`);
  console.log(`[bundle-size]   raw:      ${fmt(entry.raw)}`);
  console.log(`[bundle-size]   gzipped:  ${fmt(entry.gzipped)}`);
  console.log(`[bundle-size]   budget:   ${fmt(MAX_GZIPPED_BYTES)} gzipped`);
  if (lazy.length > 0) {
    console.log(`[bundle-size] lazy chunks (not gated):`);
    for (const c of lazy) {
      console.log(`[bundle-size]   ${c.file}: ${fmt(c.gzipped)} gzipped`);
    }
  }

  if (entry.gzipped > MAX_GZIPPED_BYTES) {
    const overage = entry.gzipped - MAX_GZIPPED_BYTES;
    console.error(
      `\n[bundle-size] FAIL: entry chunk is ${fmt(overage)} over budget.\n` +
        `[bundle-size] options: split a heavy import behind React.lazy(),\n` +
        `[bundle-size]          or bump MAX_GZIPPED_BYTES with a justification.`,
    );
    process.exit(1);
  }

  const slack = MAX_GZIPPED_BYTES - entry.gzipped;
  console.log(`[bundle-size] PASS — ${fmt(slack)} of headroom remaining.`);
}

main().catch((err) => {
  console.error(`[bundle-size] unexpected error: ${(err as Error).message}`);
  process.exit(2);
});
