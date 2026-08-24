import { describe, expect, it } from "vitest";
import { staleOverlayRoots } from "../src/overlay/controller";

type FakeOverlayRoot = { id: string };

/** Minimal page-root simulation for reload/start idempotence. */
function initializeOverlay(roots: FakeOverlayRoot[], retained: FakeOverlayRoot | undefined): FakeOverlayRoot[] {
  const stale = new Set(staleOverlayRoots(roots, retained));
  return roots.filter((root) => !stale.has(root));
}

describe("Subtitler overlay ownership", () => {
  it("keeps one host after repeated initialization", () => {
    const active = { id: "active" };
    let roots: FakeOverlayRoot[] = [active];

    roots = initializeOverlay(roots, active);
    roots = initializeOverlay(roots, active);
    roots = initializeOverlay(roots, active);

    expect(roots).toEqual([active]);
  });

  it("removes stale hosts on stop then start/reload", () => {
    const first = { id: "first" };
    const stale = { id: "stale" };
    const second = { id: "second" };

    // Stop removes the first active host. A later start/reload must not keep
    // its orphan alongside the new root.
    expect(initializeOverlay([first], undefined)).toEqual([]);
    expect(initializeOverlay([first, stale, second], second)).toEqual([second]);
  });

  it("keeps one host when an old controller re-appends its legacy root after fullscreen", () => {
    const current = { id: "current" };
    const legacyReappeared = { id: "legacy-reappeared" };

    // This is the same reconciliation the active ownership observer performs
    // when a pre-ownership controller reattaches a detached root.
    expect(initializeOverlay([current, legacyReappeared], current)).toEqual([current]);
  });
});
