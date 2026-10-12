import { create } from "zustand";
import { PS5_AVA1_PORT, PS5_LOADER_PORT } from "../lib/addr";
import { invoke } from "../lib/invokeLogged";
import { isTauriEnv } from "../lib/tauriEnv";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";
import { getEngineUrl } from "./engine";

/**
 * The two console ports, as the user set them on the Connection screen
 * (iOS fork; upstream hardcodes both):
 *
 *  - loader: the ELF loader the helper and other payloads are sent to
 *    (9021 by default — some loaders bind another port).
 *  - helper: the ps5upload helper's AVA1 port (9120 by default — another
 *    number only makes sense when a router forwards it to the console's 9120).
 *
 * Persisted to localStorage (one key each, removed when back to the default)
 * and pushed into the shell and the engine by `syncConsolePorts`, which is
 * where every connection to the console reads them (`ps5upload_core::ports`).
 */

const KEY_LOADER = "ps5upload.loader_port";
const KEY_HELPER = "ps5upload.helper_port";

/** A TCP port from what the user typed, or null when it isn't one. */
export function parsePort(text: string): number | null {
  const t = text.trim();
  if (!/^\d{1,5}$/.test(t)) return null;
  const n = Number(t);
  return n >= 1 && n <= 65535 ? n : null;
}

function loadPort(key: string, fallback: number): number {
  if (typeof window === "undefined") return fallback;
  return parsePort(safeGetItem(key) ?? "") ?? fallback;
}

function savePort(key: string, port: number, fallback: number): void {
  if (typeof window === "undefined") return;
  if (port === fallback) safeRemoveItem(key);
  else safeSetItem(key, String(port));
}

interface ConsolePortsState {
  loaderPort: number;
  helperPort: number;
  setLoaderPort: (port: number) => void;
  setHelperPort: (port: number) => void;
}

export const useConsolePortsStore = create<ConsolePortsState>((set) => ({
  loaderPort: loadPort(KEY_LOADER, PS5_LOADER_PORT),
  helperPort: loadPort(KEY_HELPER, PS5_AVA1_PORT),
  setLoaderPort: (loaderPort) => {
    savePort(KEY_LOADER, loaderPort, PS5_LOADER_PORT);
    set({ loaderPort });
    void syncConsolePorts();
  },
  setHelperPort: (helperPort) => {
    savePort(KEY_HELPER, helperPort, PS5_AVA1_PORT);
    set({ helperPort });
    void syncConsolePorts();
  },
}));

/** The loader port in effect, for non-React callers. */
export function loaderPort(): number {
  return useConsolePortsStore.getState().loaderPort;
}

/** The helper port in effect, for non-React callers. */
export function helperPort(): number {
  return useConsolePortsStore.getState().helperPort;
}

/** Push the ports into the shell (`set_console_ports`; on mobile that is the
 *  engine's process too) and the engine (`/api/console-ports`, for a desktop
 *  sidecar or a remote engine). Best-effort: a failure leaves the defaults. */
export async function syncConsolePorts(): Promise<void> {
  const { loaderPort: loader, helperPort: helper } = useConsolePortsStore.getState();
  await invoke("set_console_ports", { loader, helper }).catch(() => undefined);
  if (!isTauriEnv()) return; // browserInvoke already posted it to the engine
  await fetch(`${getEngineUrl()}/api/console-ports`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ loader, helper }),
  }).catch(() => undefined);
}
