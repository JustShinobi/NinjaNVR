import { describe, expect, test } from "bun:test";
import type { ShareLinkItem } from "../types/nvr";
import { sanitizeExportVideoUrl } from "../App";
import { sanitizeTimelineThumbUrl } from "../components/TimelineScrubber";
import { resolveSafeMediaUrl } from "../utils/mediaUrlPolicy";
import { createShareLinkSessionController } from "../utils/shareLinks";
import { resolveSafeExportVideoUrl } from "../views/ExportsWorkspace";
import { resolveSafeAlertThumbSrc } from "../views/LiveWorkspace";
import { resolveSafeSyncThumbSrc } from "../views/SyncWorkspace";

describe("mediaUrlPolicy — canonical sanitization matrix across export videos, thumbnails & event clips", () => {
  test("strips /media/<mount>/ prefixes and canonicalizes percent-encoded paths", () => {
    expect(
      resolveSafeMediaUrl("/media/nvme0/exports/front_door_100_200.mp4", {
        kind: "export_video",
        exportId: "front_door_100_200",
      })
    ).toBe("/exports/front_door_100_200.mp4");

    expect(
      resolveSafeMediaUrl("/media/pool1/clips/cam%5f1/thumb.webp", {
        kind: "thumbnail",
        fallbackCamera: "cam_1",
      })
    ).toBe("/clips/cam_1/thumb.webp");

    expect(
      resolveSafeMediaUrl("/clips/mocks/person%20walk.mp4", {
        kind: "event_clip",
        eventId: "ev_100",
      })
    ).toBe("/clips/mocks/person%20walk.mp4");

    // Empty rawPath uses safe event_clip fallback
    expect(
      resolveSafeMediaUrl("", {
        kind: "event_clip",
        eventId: "ev_100",
      })
    ).toBe("/api/events/ev_100/clip.mp4");
  });

  test("rejects traversal, encoded separators, double-encoding, control chars, external origins & sensitive /api/* routes across all media kinds", () => {
    const attackVectors = [
      "../etc/passwd",
      "/exports/../../etc/passwd",
      "/exports/%2e%2e/config.yml",
      "/exports/%252e%252e/config.yml",
      "/clips/sub%2fsecret.webp",
      "/clips/sub%5csecret.webp",
      "/clips/null%00byte.webp",
      "/clips/raw\\backslash.webp",
      "/clips/ctrl\u0007bell.webp",
      "/clips/del\u007fchar.webp",
      "/clips/malformed%ZZencoding.webp",
      "/clips//empty-segment.webp",
      "/clips/./dot-segment.webp",
      "/clips/query.webp?token=leak",
      "/clips/hash.webp#frag",
      "//evil.example/hijack.mp4",
      "https://evil.example/hijack.mp4",
      "javascript:alert(1)",
      "data:text/html,<script>alert(1)</script>",
      "/api/auth/login",
      "/api/logout",
      "/api/users",
      "/api/tokens",
      "/api/config/raw",
      "/api/restart",
      "/api/camera-proxy",
      "/api/camera-proxy/snapshot.jpg",
      "/api/federation",
      "/api/federation/peers",
      "/api/logs",
      "/api/logs/ninjanvr",
      "/api/share",
      "/api/share/shr_1",
      "/api/metrics",
      "/api/profile",
      "/api/ffprobe",
      "/api/streams",
      "/api/detectors",
      "/api/debug",
      "/api/debug/heap",
      "/api/mcp",
      "/api/ai",
      "/api/ai/context",
      "/api/go2rtc",
      "/api/go2rtc/streams",
      "/api/dataset/export.zip",
    ];

    for (const payload of attackVectors) {
      expect(
        resolveSafeMediaUrl(payload, {
          kind: "export_video",
          exportId: "safe_exp",
        })
      ).toBe("/exports/safe_exp.mp4");

      expect(
        resolveSafeMediaUrl(payload, {
          kind: "thumbnail",
          fallbackCamera: "cam_alpha",
        })
      ).toBe("/api/cam_alpha/latest.webp");

      expect(
        resolveSafeMediaUrl(payload, {
          kind: "event_clip",
          eventId: "ev_safe_1",
        })
      ).toBe("/api/events/ev_safe_1/clip.mp4");
    }

    // Delegated workspace helpers preserve exact fallback contracts
    expect(
      sanitizeExportVideoUrl("/api/tokens", "front_gate", 10, 30)
    ).toBe("/exports/front_gate_10_30.mp4");
    expect(
      resolveSafeExportVideoUrl("/api/users", "../bad_id")
    ).toBe("");
    expect(sanitizeTimelineThumbUrl("/api/restart", "lobby")).toBe(
      "/api/lobby/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("/api/auth/providers", "patio")).toBe(
      "/api/patio/latest.webp"
    );
    expect(resolveSafeSyncThumbSrc("/api/config", "garage")).toBe(
      "/api/garage/latest.webp"
    );
    expect(sanitizeTimelineThumbUrl("/api/dataset/export.zip", "lobby")).toBe(
      "/api/lobby/latest.webp"
    );
    expect(sanitizeTimelineThumbUrl("/api/camera-proxy/stream", "lobby")).toBe(
      "/api/lobby/latest.webp"
    );
    expect(
      sanitizeTimelineThumbUrl("/clips/raw space/thumb.webp", "lobby")
    ).toBe("/api/lobby/latest.webp");
    expect(
      sanitizeTimelineThumbUrl("/clips/encoded%20space/thumb.webp", "lobby")
    ).toBe("/clips/encoded%20space/thumb.webp");

    // Legitimate /api/* media endpoints remain allowed
    expect(
      resolveSafeMediaUrl("/api/escritorio/latest.webp", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/escritorio/latest.webp");
    expect(
      resolveSafeMediaUrl("/api/escritorio/latest.jpg", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/escritorio/latest.jpg");
    expect(
      resolveSafeMediaUrl("/api/escritorio/snapshot.jpg", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/escritorio/snapshot.jpg");
    expect(
      resolveSafeMediaUrl("/api/events/ev_100/thumbnail.webp", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/events/ev_100/thumbnail.webp");
    expect(
      resolveSafeMediaUrl("/api/review/rev_1/preview.gif", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/review/rev_1/preview.gif");
    expect(
      resolveSafeMediaUrl("/api/preview/prev_1/thumbnail.jpg", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/preview/prev_1/thumbnail.jpg");
    expect(
      resolveSafeMediaUrl("/api/exports/exp_1.mp4", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/exports/exp_1.mp4");
    expect(
      resolveSafeMediaUrl("/api/dataset/samples/smp_1/image", {
        kind: "thumbnail",
        fallbackCamera: "fallback_cam",
      })
    ).toBe("/api/dataset/samples/smp_1/image");
  });
});

describe("createShareLinkSessionController — token URL caching, rotation, revocation & target eviction", () => {
  const nowSec = 1_700_100_000;
  const origin = "https://nvr.example.test";

  function makeLink(overrides: Partial<ShareLinkItem> = {}): ShareLinkItem {
    return {
      id: "shr_1",
      kind: "export",
      target_id: "exp_alpha",
      camera: "escritorio",
      created_by: "admin",
      created_at: nowSec - 100,
      expires_at: nowSec + 3600,
      revoked: false,
      ...overrides,
    };
  }

  test("createAndCopy caches one-time token URL, copies to clipboard, and enforces busy guard", async () => {
    let serverLinks: ShareLinkItem[] = [];
    const copied: Array<{ url: string; ttl?: string }> = [];
    const notes: string[] = [];

    const ctrl = createShareLinkSessionController({
      origin,
      nowSec: () => nowSec,
      listShareLinks: async () => serverLinks,
      copyUrlToClipboard: async (url, ttl) => {
        copied.push({ url, ttl });
      },
      notify: (m) => notes.push(m),
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/api/share" && init?.method === "POST") {
          const created = makeLink({ id: "shr_created_1", target_id: "exp_alpha" });
          serverLinks = [created];
          return new Response(
            JSON.stringify({
              id: "shr_created_1",
              token: "tok_one_time_secret.sig",
            }),
            { status: 200 }
          );
        }
        throw new Error(`Unexpected fetch: ${url}`);
      }) as unknown as typeof fetch,
    });

    const res = await ctrl.createAndCopy({
      targetId: "exp_alpha",
      expiresInSec: 3600,
      maxViews: 5,
    });

    expect(res).toEqual({
      ok: true,
      url: "https://nvr.example.test/share/tok_one_time_secret.sig",
    });
    expect(ctrl.getState().shareUrlMap["shr_created_1"]).toBe(
      "https://nvr.example.test/share/tok_one_time_secret.sig"
    );
    expect(ctrl.getState().generatedShareUrl).toBe(
      "https://nvr.example.test/share/tok_one_time_secret.sig"
    );
    expect(copied).toEqual([
      {
        url: "https://nvr.example.test/share/tok_one_time_secret.sig",
        ttl: "1h",
      },
    ]);
    expect(notes).toEqual([]);

    // Subsequent copyOrRotate reuses cached shareUrlMap entry without rotating when server confirms active
    const reuseRes = await ctrl.copyOrRotate(serverLinks[0]);
    expect(reuseRes).toEqual({
      ok: true,
      url: "https://nvr.example.test/share/tok_one_time_secret.sig",
      rotated: false,
    });
    expect(copied.length).toBe(2);
  });

  test("copyOrRotate rotates active uncached links, revoke evicts token, and evictTarget cleans up deleted export links", async () => {
    const uncachedActive = makeLink({
      id: "shr_old_uncached",
      target_id: "exp_target_9",
      expires_at: nowSec + 1800,
      max_views: 5,
      access_count: 2,
    });
    let serverLinks: ShareLinkItem[] = [uncachedActive];
    const copied: string[] = [];
    const notes: string[] = [];
    const apiCalls: string[] = [];

    const ctrl = createShareLinkSessionController({
      origin,
      nowSec: () => nowSec,
      listShareLinks: async () => serverLinks,
      copyUrlToClipboard: async (url) => {
        copied.push(url);
      },
      notify: (m) => notes.push(m),
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method || "GET";
        apiCalls.push(`${method} ${url}`);
        if (method === "DELETE" && url === "/api/share/shr_old_uncached") {
          serverLinks = [];
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (method === "POST" && url === "/api/share") {
          const rotated = makeLink({
            id: "shr_rotated_2",
            target_id: "exp_target_9",
            expires_at: nowSec + 1800,
            max_views: 3,
          });
          serverLinks = [rotated];
          return new Response(
            JSON.stringify({
              id: "shr_rotated_2",
              url: "/share/tok_rotated_2.sig",
            }),
            { status: 200 }
          );
        }
        if (method === "DELETE" && url === "/api/share/shr_to_revoke") {
          serverLinks = serverLinks.filter((l) => l.id !== "shr_to_revoke");
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        throw new Error(`Unexpected call: ${method} ${url}`);
      }) as unknown as typeof fetch,
    });

    const rot = await ctrl.copyOrRotate(uncachedActive);
    expect(rot).toEqual({
      ok: true,
      url: "https://nvr.example.test/share/tok_rotated_2.sig",
      rotated: true,
    });
    expect(apiCalls).toEqual([
      "DELETE /api/share/shr_old_uncached",
      "POST /api/share",
    ]);
    expect(ctrl.getState().shareUrlMap).toEqual({
      shr_rotated_2: "https://nvr.example.test/share/tok_rotated_2.sig",
    });

    // Seed generatedShareUrl to rotated URL, then evictTarget("exp_target_9") when export is deleted
    ctrl.seedState({
      generatedShareUrl: "https://nvr.example.test/share/tok_rotated_2.sig",
    });
    const eviction = ctrl.evictTarget("exp_target_9");
    expect(eviction).toEqual({
      evictedIds: ["shr_rotated_2"],
      clearedGeneratedUrl: true,
    });
    expect(ctrl.getState().shareUrlMap).toEqual({});
    expect(ctrl.getState().generatedShareUrl).toBe("");

    // Revoke flow evicts revoked link URL and notifies
    const extraLink = makeLink({ id: "shr_to_revoke", target_id: "exp_other" });
    serverLinks = [extraLink];
    ctrl.seedState({
      links: [extraLink],
      shareUrlMap: {
        shr_to_revoke: "https://nvr.example.test/share/tok_revoke.sig",
      },
      generatedShareUrl: "https://nvr.example.test/share/tok_revoke.sig",
    });

    const revokedOk = await ctrl.revoke("shr_to_revoke");
    expect(revokedOk).toBe(true);
    expect(ctrl.getState().shareUrlMap).toEqual({});
    expect(ctrl.getState().generatedShareUrl).toBe("");
    expect(notes).toContain("Revoked share link shr_to_revoke");
  });

  test("StrictMode subscribe -> dispose -> subscribe revives session and clears creatingShareId / revokingShareId even if disposed mid-flight", async () => {
    let serverLinks: ShareLinkItem[] = [
      makeLink({ id: "shr_initial", target_id: "exp_strict" }),
    ];
    const snapshots: Array<ReturnType<typeof ctrl.getState>> = [];
    const notes: string[] = [];
    let releaseInFlightPost: (() => void) | null = null;

    const ctrl = createShareLinkSessionController({
      origin,
      nowSec: () => nowSec,
      listShareLinks: async () => serverLinks,
      copyUrlToClipboard: async () => {},
      notify: (m) => notes.push(m),
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method || "GET";
        if (method === "POST" && url === "/api/share") {
          if (releaseInFlightPost) {
            await new Promise<void>((resolve) => {
              releaseInFlightPost = resolve;
            });
          }
          const created = makeLink({
            id: `shr_new_${serverLinks.length + 1}`,
            target_id: "exp_strict",
          });
          serverLinks = [created, ...serverLinks];
          return new Response(
            JSON.stringify({
              id: created.id,
              token: `tok_${created.id}.sig`,
            }),
            { status: 200 }
          );
        }
        if (method === "DELETE" && url.startsWith("/api/share/")) {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        throw new Error(`Unexpected fetch: ${method} ${url}`);
      }) as unknown as typeof fetch,
    });

    // 1. Mount #1 (StrictMode initial mount) -> unmount (dispose) -> Mount #2 (StrictMode remount)
    const unsub1 = ctrl.subscribe(() => {});
    unsub1();
    ctrl.dispose();

    const unsub2 = ctrl.subscribe((snap) => {
      snapshots.push(snap);
    });

    const refreshed = await ctrl.refresh();
    expect(refreshed?.map((l) => l.id)).toEqual(["shr_initial"]);
    expect(ctrl.getState().links.map((l) => l.id)).toEqual(["shr_initial"]);
    expect(snapshots.length).toBeGreaterThan(0);

    const createdRes = await ctrl.createAndCopy({
      targetId: "exp_strict",
      expiresInSec: 3600,
    });
    expect(createdRes).toEqual({
      ok: true,
      url: "https://nvr.example.test/share/tok_shr_new_2.sig",
    });
    expect(ctrl.getState().creatingShareId).toBeNull();

    // 2. Dispose mid-flight during createAndCopy -> creatingShareId is still cleared in finally without emitting while disposed
    let proceed!: () => void;
    releaseInFlightPost = () => {};
    const gate = new Promise<void>((r) => {
      proceed = r;
    });
    const ctrlMidFlight = createShareLinkSessionController({
      origin,
      nowSec: () => nowSec,
      listShareLinks: async () => serverLinks,
      copyUrlToClipboard: async () => {},
      notify: (m) => notes.push(m),
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method || "GET";
        if (method === "POST" && url === "/api/share") {
          await gate;
          return new Response(
            JSON.stringify({ id: "shr_mid", token: "tok_mid.sig" }),
            { status: 200 }
          );
        }
        throw new Error(`Unexpected fetch: ${method} ${url}`);
      }) as unknown as typeof fetch,
    });

    const disposedEvents: string[] = [];
    const unsubMid1 = ctrlMidFlight.subscribe((s) => {
      disposedEvents.push(`mid1:${s.creatingShareId}`);
    });
    const inFlightCreate = ctrlMidFlight.createAndCopy({
      targetId: "exp_mid",
      expiresInSec: 3600,
    });
    expect(ctrlMidFlight.getState().creatingShareId).toBe("exp_mid");
    unsubMid1();
    ctrlMidFlight.dispose();
    const countWhileDisposed = disposedEvents.length;
    const notesBeforeFinish = notes.length;

    proceed();
    const midRes = await inFlightCreate;
    expect(midRes).toEqual({ ok: false, url: "" });
    expect(ctrlMidFlight.getState().creatingShareId).toBeNull();
    expect(disposedEvents.length).toBe(countWhileDisposed);
    expect(notes.length).toBe(notesBeforeFinish);

    // Re-subscribing after mid-flight disposal works cleanly and is not stuck on creatingShareId
    const remountSnaps: Array<string | null> = [];
    const unsubMid2 = ctrlMidFlight.subscribe((s) => {
      remountSnaps.push(s.creatingShareId);
    });
    const retryRes = await ctrlMidFlight.createAndCopy({
      targetId: "exp_after_remount",
      expiresInSec: 3600,
    });
    expect(retryRes).toEqual({
      ok: true,
      url: "https://nvr.example.test/share/tok_mid.sig",
    });
    expect(ctrlMidFlight.getState().creatingShareId).toBeNull();
    expect(remountSnaps).toContain("exp_after_remount");
    expect(remountSnaps[remountSnaps.length - 1]).toBeNull();

    // 3. Dispose mid-flight during revoke -> revokingShareId is cleared in finally
    let proceedRevoke!: () => void;
    const revokeGate = new Promise<void>((r) => {
      proceedRevoke = r;
    });
    const ctrlRevoke = createShareLinkSessionController({
      origin,
      nowSec: () => nowSec,
      listShareLinks: async () => serverLinks,
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method || "GET";
        if (method === "DELETE" && url === "/api/share/shr_rev_mid") {
          await revokeGate;
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        throw new Error(`Unexpected fetch: ${method} ${url}`);
      }) as unknown as typeof fetch,
    });
    const unsubRev = ctrlRevoke.subscribe(() => {});
    const inFlightRevoke = ctrlRevoke.revoke("shr_rev_mid");
    expect(ctrlRevoke.getState().revokingShareId).toBe("shr_rev_mid");
    unsubRev();
    ctrlRevoke.dispose();
    proceedRevoke();
    expect(await inFlightRevoke).toBe(false);
    expect(ctrlRevoke.getState().revokingShareId).toBeNull();

    unsubMid2();
    unsub2();
  });
});
