import os from "node:os";
import path from "node:path";
export const APP_NAME = "tachi";
export function resolveUserPath(value) {
    return path.resolve(value.replace(/^~/, os.homedir()));
}
export const appHome = resolveUserPath(process.env.TACHI_HOME || process.env.SIGIL_HOME || `~/.${APP_NAME}`);
export const defaultDbPath = path.resolve(appHome, "memory.db");
