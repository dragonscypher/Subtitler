import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { JobStore, jobsForTab } from "../src/background/job-store";

describe("JobStore", () => {
  it("keeps a popup scoped to the active recording tab", async () => {
    const store = new JobStore();
    await store.initialize();
    const first = await store.create({ id: "first", kind: "transcript", tabId: 10 });
    const second = await store.create({ id: "second", kind: "subtitle", tabId: 20 });

    expect(jobsForTab([first, second], 20)).toEqual([second]);
    expect(jobsForTab([first, second], 99)).toEqual([]);
  });

  let storage: Record<string, unknown>;

  beforeEach(() => {
    storage = {};
    (globalThis as unknown as { chrome: typeof chrome }).chrome = {
      runtime: { lastError: undefined },
      storage: {
        local: {
          get: (key: string, callback: (items: Record<string, unknown>) => void) => callback({ [key]: storage[key] }),
          set: (items: Record<string, unknown>, callback: () => void) => {
            Object.assign(storage, items);
            callback();
          }
        }
      }
    } as unknown as typeof chrome;
  });

  afterEach(() => {
    delete (globalThis as unknown as { chrome?: typeof chrome }).chrome;
  });

  it("keeps a user-stopped job terminal when late native state arrives", async () => {
    const store = new JobStore();
    await store.initialize();
    await store.create({ id: "extension-stopped-job", kind: "subtitle" });
    await store.update("extension-stopped-job", { status: "stopped" });

    const late = await store.update("extension-stopped-job", {
      status: "processing",
      nativeJobId: "22222222-2222-4222-8222-222222222222",
      progress: { processedSeconds: 30 }
    });

    expect(late).toMatchObject({ id: "extension-stopped-job", status: "stopped" });
    expect(late?.nativeJobId).toBeUndefined();
    expect(late?.progress).toBeUndefined();
    expect(store.get("extension-stopped-job")).toEqual(late);
  });
});
