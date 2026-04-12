import { watch } from "chokidar";
import { PLANS_DIR } from "./config.js";
import { parsePlanFile } from "./plan-parser.js";
import { broadcast } from "./ws.js";

export function startFileWatcher() {
  const watcher = watch(`${PLANS_DIR}/*.md`, {
    ignoreInitial: true,
    awaitWriteFinish: { stabilityThreshold: 300, pollInterval: 100 },
  });

  watcher.on("add", (filePath) => {
    console.log(`[watcher] Plan added: ${filePath}`);
    try {
      const plan = parsePlanFile(filePath);
      broadcast("plan_updated", { action: "added", plan });
    } catch (e) {
      console.error(`[watcher] Failed to parse ${filePath}:`, e);
    }
  });

  watcher.on("change", (filePath) => {
    console.log(`[watcher] Plan changed: ${filePath}`);
    try {
      const plan = parsePlanFile(filePath);
      broadcast("plan_updated", { action: "changed", plan });
    } catch (e) {
      console.error(`[watcher] Failed to parse ${filePath}:`, e);
    }
  });

  watcher.on("unlink", (filePath) => {
    console.log(`[watcher] Plan removed: ${filePath}`);
    const name = filePath.split("/").pop()?.replace(".md", "") ?? "";
    broadcast("plan_updated", { action: "removed", name });
  });

  console.log(`[watcher] Watching ${PLANS_DIR}`);
  return watcher;
}
