import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchConsoleStat, importLocalCheat, minionTile } from "./consoleFiles";
vi.mock("../state/engine", () => ({ getEngineUrl: () => "http://engine" }));
afterEach(() => vi.unstubAllGlobals());
function reply(body: object, status = 200) {
  const fetcher = vi
    .fn()
    .mockImplementation(
      async (_input: RequestInfo | URL, _init?: RequestInit) =>
        new Response(JSON.stringify(body), { status }),
    );
  vi.stubGlobal("fetch", fetcher);
  return fetcher;
}
describe("paired console controls", () => {
  it("reads and sets Minion without changing LAN access", async () => {
    const fetcher = reply({
      supported: true,
      tile: true,
      tile_busy: true,
      tile_failed: false,
    });
    await minionTile("10.0.0.2", true);
    expect(fetcher.mock.calls[0][0]).toBe("http://engine/api/ps5/minion/tile");
    expect(JSON.parse(fetcher.mock.calls[0][1].body)).toEqual({
      addr: "10.0.0.2",
      tile: true,
    });
    await minionTile("10.0.0.2");
    expect(JSON.parse(fetcher.mock.calls[1][1].body)).not.toHaveProperty(
      "tile",
    );
  });
  it("feature detects old helpers and rejects malformed settings", async () => {
    reply({ supported: false });
    expect(await minionTile("h")).toEqual({ supported: false });
    reply({ supported: true });
    await expect(minionTile("h")).rejects.toThrow("Invalid Minion");
  });
  it("rejects invalid stats and surfaces helper errors", async () => {
    reply({ kind: "file", mode: "0644" });
    await expect(fetchConsoleStat("h", "/data/x")).rejects.toThrow(
      "Invalid file stat",
    );
    reply({ error: "permission denied" }, 502);
    await expect(fetchConsoleStat("h", "/data/x")).rejects.toThrow(
      "permission denied",
    );
  });
  it("imports via the engine and requires a confirmed destination", async () => {
    const fetcher = reply({
      ok: true,
      path: "/data/ps5upload/cheats/json/PPSA00001.json",
    });
    expect((await importLocalCheat("h", "/tmp/PPSA00001.json")).path).toContain(
      "PPSA00001.json",
    );
    expect(JSON.parse(fetcher.mock.calls[0][1].body).path).toBe(
      "/tmp/PPSA00001.json",
    );
    reply({ ok: false, error: "already exists" });
    await expect(importLocalCheat("h", "/tmp/PPSA00001.json")).rejects.toThrow(
      "already exists",
    );
  });
});
