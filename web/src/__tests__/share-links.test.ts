import { describe, expect, test } from "bun:test";
import type { ShareLinkItem } from "../types/nvr";
import {
  buildSharePublicUrl,
  computeShareLinkRotationParams,
  computeShareLinkStatus,
  filterActiveAndRecentShareLinks,
  formatShareLinkRemainingTtl,
  formatShareLinkViews,
  isValidShareTokenSegment,
} from "../utils/shareLinks";
import { executeCopyExistingShareLink } from "../views/ExportsWorkspace";

function makeShareLink(overrides: Partial<ShareLinkItem> = {}): ShareLinkItem {
  return {
    id: "shr_1700000000_00000001",
    kind: "export",
    target_id: "front_door_1700000000_1700000060",
    camera: "front_door",
    created_by: "admin",
    created_at: 1700000000,
    expires_at: 1700086400,
    revoked: false,
    ...overrides,
  };
}

describe("shareLinks utilities", () => {
  const nowSec = 1700001000;

  test("computeShareLinkStatus classifies active, revoked, expired, and exhausted links across all edge cases", () => {
    // Active without max_views
    expect(computeShareLinkStatus(makeShareLink(), nowSec)).toBe("active");

    // Active with remaining views
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 2, max_views: 5 }),
        nowSec
      )
    ).toBe("active");

    // Active when max_views is null, undefined, or 0 (unlimited)
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 100, max_views: null }),
        nowSec
      )
    ).toBe("active");
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 100, max_views: 0 }),
        nowSec
      )
    ).toBe("active");

    // Exhausted when access_count reaches or exceeds finite max_views
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 1, max_views: 1 }),
        nowSec
      )
    ).toBe("exhausted");
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 5, max_views: 5 }),
        nowSec
      )
    ).toBe("exhausted");
    expect(
      computeShareLinkStatus(
        makeShareLink({ access_count: 8, max_views: 5 }),
        nowSec
      )
    ).toBe("exhausted");

    // Expired at exact boundary and in the past
    expect(
      computeShareLinkStatus(makeShareLink({ expires_at: nowSec }), nowSec)
    ).toBe("expired");
    expect(
      computeShareLinkStatus(makeShareLink({ expires_at: nowSec - 1 }), nowSec)
    ).toBe("expired");
    expect(
      computeShareLinkStatus(makeShareLink({ expires_at: Number.NaN }), nowSec)
    ).toBe("expired");

    // Revoked takes precedence over active, expired, and exhausted states
    expect(
      computeShareLinkStatus(makeShareLink({ revoked: true }), nowSec)
    ).toBe("revoked");
    expect(
      computeShareLinkStatus(
        makeShareLink({
          revoked: true,
          expires_at: nowSec - 500,
          access_count: 5,
          max_views: 5,
        }),
        nowSec
      )
    ).toBe("revoked");

    // Default nowSec fallback works for future links
    const futureSec = Math.floor(Date.now() / 1000) + 3600;
    expect(
      computeShareLinkStatus(makeShareLink({ expires_at: futureSec }))
    ).toBe("active");
  });

  test("formatShareLinkRemainingTtl formats expired, seconds, minutes, hours, and days", () => {
    expect(formatShareLinkRemainingTtl(nowSec, nowSec)).toBe("Expired");
    expect(formatShareLinkRemainingTtl(nowSec + 0.4, nowSec)).toBe("Expired");
    expect(formatShareLinkRemainingTtl(nowSec - 120, nowSec)).toBe("Expired");
    expect(formatShareLinkRemainingTtl(Number.NaN, nowSec)).toBe("Expired");

    expect(formatShareLinkRemainingTtl(nowSec + 1, nowSec)).toBe("1s");
    expect(formatShareLinkRemainingTtl(nowSec + 45, nowSec)).toBe("45s");

    expect(formatShareLinkRemainingTtl(nowSec + 60, nowSec)).toBe("1m");
    expect(formatShareLinkRemainingTtl(nowSec + 95, nowSec)).toBe("1m 35s");

    expect(formatShareLinkRemainingTtl(nowSec + 3600, nowSec)).toBe("1h");
    expect(formatShareLinkRemainingTtl(nowSec + 5400, nowSec)).toBe("1h 30m");

    expect(formatShareLinkRemainingTtl(nowSec + 86400, nowSec)).toBe("1d");
    expect(formatShareLinkRemainingTtl(nowSec + 93600, nowSec)).toBe("1d 2h");
    expect(formatShareLinkRemainingTtl(nowSec + 604800, nowSec)).toBe("7d");

    const futureSec = Math.floor(Date.now() / 1000) + 7200;
    expect(formatShareLinkRemainingTtl(futureSec)).toMatch(/^1h 59m|2h$/);
  });

  test("filterActiveAndRecentShareLinks filters inactive links when requested and sorts newest first without mutating input", () => {
    expect(filterActiveAndRecentShareLinks([], false, nowSec)).toEqual([]);

    const activeOlder = makeShareLink({
      id: "shr_1",
      created_at: 1700000100,
      expires_at: nowSec + 3600,
    });
    const activeNewer = makeShareLink({
      id: "shr_2",
      created_at: 1700000500,
      expires_at: nowSec + 7200,
    });
    const revokedLink = makeShareLink({
      id: "shr_3",
      created_at: 1700000900,
      expires_at: nowSec + 7200,
      revoked: true,
    });
    const expiredLink = makeShareLink({
      id: "shr_4",
      created_at: 1700000300,
      expires_at: nowSec - 10,
    });
    const exhaustedLink = makeShareLink({
      id: "shr_5",
      created_at: 1700000700,
      expires_at: nowSec + 7200,
      access_count: 5,
      max_views: 5,
    });

    const input = [
      activeOlder,
      revokedLink,
      expiredLink,
      activeNewer,
      exhaustedLink,
    ];
    const snapshot = [...input];

    const activeOnly = filterActiveAndRecentShareLinks(input, false, nowSec);
    expect(activeOnly.map((l) => l.id)).toEqual(["shr_2", "shr_1"]);

    const withInactive = filterActiveAndRecentShareLinks(input, true, nowSec);
    expect(withInactive.map((l) => l.id)).toEqual([
      "shr_3",
      "shr_5",
      "shr_2",
      "shr_4",
      "shr_1",
    ]);

    // Deterministic tie-breaker when created_at timestamps are equal
    const tieA = makeShareLink({ id: "shr_a", created_at: 1700000500 });
    const tieB = makeShareLink({ id: "shr_b", created_at: 1700000500 });
    expect(
      filterActiveAndRecentShareLinks([tieA, tieB], false, nowSec).map(
        (l) => l.id
      )
    ).toEqual(["shr_b", "shr_a"]);

    // Original array is untouched
    expect(input).toEqual(snapshot);
  });

  test("buildSharePublicUrl normalizes origins, relative paths, bare tokens, and full URLs while rejecting cross-origin and unsafe targets", () => {
    expect(
      buildSharePublicUrl("http://192.168.1.50:5000", "/share/tok.sig")
    ).toBe("http://192.168.1.50:5000/share/tok.sig");
    expect(
      buildSharePublicUrl("https://nvr.example.test///", "/share/tok.sig")
    ).toBe("https://nvr.example.test/share/tok.sig");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "share/tok.sig")
    ).toBe("https://nvr.example.test/share/tok.sig");
    expect(buildSharePublicUrl("https://nvr.example.test", "tok.sig")).toBe(
      "https://nvr.example.test/share/tok.sig"
    );
    expect(buildSharePublicUrl("", "/share/tok.sig")).toBe("/share/tok.sig");
    expect(buildSharePublicUrl("", "tok.sig")).toBe("/share/tok.sig");

    // Same-origin full URLs are accepted
    expect(
      buildSharePublicUrl(
        "https://nvr.example.test",
        "https://nvr.example.test/share/tok.sig"
      )
    ).toBe("https://nvr.example.test/share/tok.sig");
    expect(
      buildSharePublicUrl("", "https://nvr.example.test/share/tok.sig")
    ).toBe("https://nvr.example.test/share/tok.sig");

    // Cross-origin full URLs are rejected
    expect(
      buildSharePublicUrl(
        "http://localhost:5000",
        "https://evil.example/share/tok.sig"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.example.test",
        "https://evil.example/share/tok.sig"
      )
    ).toBe("");

    // Protocol-relative URLs, non-/share/ paths, javascript: URIs, whitespace/control chars, and empty /share/ are rejected
    expect(
      buildSharePublicUrl("https://nvr.example.test", "//evil.example/share/tok.sig")
    ).toBe("");
    expect(buildSharePublicUrl("", "//evil.example/share/tok.sig")).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "/api/logout")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "/custom/path")
    ).toBe("");
    expect(buildSharePublicUrl("", "/custom/path")).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.example.test",
        "https://nvr.example.test/api/logout"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "javascript:alert(1)")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "tok.sig\nwith-newline")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "/share/tok with space")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.example.test", "/share/\u0000tok")
    ).toBe("");
    expect(buildSharePublicUrl("https://nvr.example.test", "/share/")).toBe("");
    expect(buildSharePublicUrl("https://nvr.example.test", "/share//tok")).toBe("");
    expect(buildSharePublicUrl("https://nvr.example.test", "share/")).toBe("");
    expect(buildSharePublicUrl("https://nvr.example.test", "share//tok")).toBe("");
    expect(buildSharePublicUrl("http://localhost:5000", "   ")).toBe("");

    // Single-segment token validation rejects path traversal, backslashes, extra segments, query strings, and hashes across all branches
    expect(isValidShareTokenSegment("tok.sig")).toBe(true);
    expect(isValidShareTokenSegment("")).toBe(false);
    expect(isValidShareTokenSegment("..")).toBe(false);
    expect(isValidShareTokenSegment("tok..sig")).toBe(false);
    expect(isValidShareTokenSegment("tok/extra")).toBe(false);
    expect(isValidShareTokenSegment("tok\\@evil")).toBe(false);
    expect(isValidShareTokenSegment("tok?foo=bar")).toBe(false);
    expect(isValidShareTokenSegment("tok#hash")).toBe(false);
    expect(isValidShareTokenSegment("tok:colon")).toBe(false);

    expect(
      buildSharePublicUrl("https://nvr.local", "/share/../../admin")
    ).toBe("");
    expect(buildSharePublicUrl("https://nvr.local", "/share/tok\\@evil")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "/share/tok/extra")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "/share/tok?foo=bar")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "/share/tok#frag")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "share/../../admin")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "share/tok\\@evil")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "share/tok/extra")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "share/tok?foo=bar")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local", "tok..sig")).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/tok/extra"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/tok?foo=bar"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/tok#frag"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/../../admin"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/tok\\@evil"
      )
    ).toBe("");

    // Malformed, non-http(s), credentialed, subpath, query, or hash origins are rejected across relative, bare, and full URL inputs
    expect(buildSharePublicUrl("javascript:alert(1)", "/share/tok.sig")).toBe(
      ""
    );
    expect(buildSharePublicUrl("javascript:alert(1)", "tok.sig")).toBe("");
    expect(
      buildSharePublicUrl("https://user:pass@nvr.local", "/share/tok.sig")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://user:pass@nvr.local", "tok.sig")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://user@nvr.local", "/share/tok.sig")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://:pass@nvr.local", "/share/tok.sig")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.local/subpath", "/share/tok.sig")
    ).toBe("");
    expect(buildSharePublicUrl("https://nvr.local/subpath", "tok.sig")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local?q=1", "/share/tok.sig")).toBe(
      ""
    );
    expect(buildSharePublicUrl("https://nvr.local?q=1", "tok.sig")).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.local#frag", "/share/tok.sig")
    ).toBe("");
    expect(buildSharePublicUrl("not a valid origin", "/share/tok.sig")).toBe(
      ""
    );

    // Credentialed full URLs are rejected whether origin is provided or empty
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://user:pass@nvr.local/share/tok"
      )
    ).toBe("");
    expect(
      buildSharePublicUrl("", "https://user:pass@nvr.local/share/tok")
    ).toBe("");
    expect(
      buildSharePublicUrl("", "https://user@nvr.local/share/tok")
    ).toBe("");
    expect(
      buildSharePublicUrl("", "https://:pass@nvr.local/share/tok")
    ).toBe("");
  });

  test("formatShareLinkViews formats bounded and unlimited view counts", () => {
    expect(formatShareLinkViews({ access_count: undefined, max_views: null })).toBe(
      "0"
    );
    expect(formatShareLinkViews({ access_count: -4, max_views: 0 })).toBe("0");
    expect(formatShareLinkViews({ access_count: 3, max_views: -5 })).toBe("3");
    expect(formatShareLinkViews({ access_count: 3.9, max_views: 5.8 })).toBe(
      "3/5"
    );
    expect(formatShareLinkViews({ access_count: 3, max_views: undefined })).toBe(
      "3"
    );
    expect(formatShareLinkViews({ access_count: 2, max_views: 5 })).toBe("2/5");
    expect(formatShareLinkViews({ access_count: 1, max_views: 1 })).toBe("1/1");
  });

  test("computeShareLinkRotationParams preserves remaining TTL, view quota, and old link revocation target", () => {
    // Active link without max_views (unlimited)
    const unlimitedLink = makeShareLink({
      id: "shr_unlim",
      expires_at: nowSec + 3600,
      max_views: null,
      access_count: 12,
    });
    expect(computeShareLinkRotationParams(unlimitedLink, nowSec)).toEqual({
      expiresInSec: 3600,
      remainingMaxViews: null,
      revokeOldId: "shr_unlim",
    });

    // Preserves exact expiresInSec without artificial 60s floor extension when < 60s remaining
    const nearExpiryLink = makeShareLink({
      id: "shr_near",
      expires_at: nowSec + 25,
    });
    expect(computeShareLinkRotationParams(nearExpiryLink, nowSec)).toEqual({
      expiresInSec: 25,
      remainingMaxViews: null,
      revokeOldId: "shr_near",
    });

    const oneSecLeftLink = makeShareLink({
      id: "shr_one_sec",
      expires_at: nowSec + 1.8,
    });
    expect(computeShareLinkRotationParams(oneSecLeftLink, nowSec)).toEqual({
      expiresInSec: 1,
      remainingMaxViews: null,
      revokeOldId: "shr_one_sec",
    });

    // Sub-second remaining TTL (floor 0) returns null instead of extending TTL
    expect(
      computeShareLinkRotationParams(
        makeShareLink({ id: "shr_subsec_9", expires_at: nowSec + 0.9 }),
        nowSec
      )
    ).toBeNull();
    expect(
      computeShareLinkRotationParams(
        makeShareLink({ id: "shr_subsec_1", expires_at: nowSec + 0.1 }),
        nowSec
      )
    ).toBeNull();

    // Active link with max_views: 5 and access_count: 3 -> remainingMaxViews: 2
    const partialQuotaLink = makeShareLink({
      id: "shr_quota_partial",
      expires_at: nowSec + 7200,
      max_views: 5,
      access_count: 3,
    });
    expect(computeShareLinkRotationParams(partialQuotaLink, nowSec)).toEqual({
      expiresInSec: 7200,
      remainingMaxViews: 2,
      revokeOldId: "shr_quota_partial",
    });

    // Active link with max_views: 5 and access_count: 4 -> remainingMaxViews: 1
    expect(
      computeShareLinkRotationParams(
        makeShareLink({
          id: "shr_quota_last",
          expires_at: nowSec + 1800,
          max_views: 5,
          access_count: 4,
        }),
        nowSec
      )
    ).toEqual({
      expiresInSec: 1800,
      remainingMaxViews: 1,
      revokeOldId: "shr_quota_last",
    });

    // Active link with max_views: 5 and access_count: 0 or undefined -> remainingMaxViews: 5
    expect(
      computeShareLinkRotationParams(
        makeShareLink({
          id: "shr_quota_zero",
          expires_at: nowSec + 1200,
          max_views: 5,
          access_count: 0,
        }),
        nowSec
      )
    ).toEqual({
      expiresInSec: 1200,
      remainingMaxViews: 5,
      revokeOldId: "shr_quota_zero",
    });
    expect(
      computeShareLinkRotationParams(
        makeShareLink({
          id: "shr_quota_undef",
          expires_at: nowSec + 1200,
          max_views: 5,
          access_count: undefined,
        }),
        nowSec
      )
    ).toEqual({
      expiresInSec: 1200,
      remainingMaxViews: 5,
      revokeOldId: "shr_quota_undef",
    });

    // Inactive links (revoked, expired, exhausted) return null
    expect(
      computeShareLinkRotationParams(
        makeShareLink({ id: "shr_revoked", revoked: true, expires_at: nowSec + 3600 }),
        nowSec
      )
    ).toBeNull();
    expect(
      computeShareLinkRotationParams(
        makeShareLink({ id: "shr_expired", expires_at: nowSec }),
        nowSec
      )
    ).toBeNull();
    expect(
      computeShareLinkRotationParams(
        makeShareLink({ id: "shr_expired_past", expires_at: nowSec - 100 }),
        nowSec
      )
    ).toBeNull();
    expect(
      computeShareLinkRotationParams(
        makeShareLink({
          id: "shr_exhausted_eq",
          expires_at: nowSec + 3600,
          max_views: 5,
          access_count: 5,
        }),
        nowSec
      )
    ).toBeNull();
    expect(
      computeShareLinkRotationParams(
        makeShareLink({
          id: "shr_exhausted_gt",
          expires_at: nowSec + 3600,
          max_views: 5,
          access_count: 7,
        }),
        nowSec
      )
    ).toBeNull();
  });

  test("executeCopyExistingShareLink always refreshes server state before copying cached knownUrl and evicts exhausted or revoked links", async () => {
    const futureExpires = Math.floor(Date.now() / 1000) + 3600;
    const staleLocalLink = makeShareLink({
      id: "shr_cached_1",
      target_id: "exp_1",
      expires_at: futureExpires,
      max_views: 1,
      access_count: 0,
      revoked: false,
    });
    const cachedUrl = "https://nvr.example.test/share/tok_cached_1.sig";

    // 1. Server refresh reveals max_views=1 was already exhausted (access_count=1):
    // must NOT copy cachedUrl, must evict link.id from shareUrlMap, clear generatedShareUrl, and notify inactive
    let shareUrlMap: Record<string, string> = {
      shr_cached_1: cachedUrl,
      shr_other: "https://nvr.example.test/share/tok_other.sig",
    };
    let generatedShareUrl = cachedUrl;
    const copiedUrls: string[] = [];
    const notifications: string[] = [];
    const creatingTransitions: Array<string | null> = [];
    let fetchCalls = 0;
    let fetchShareLinksCalls = 0;

    await executeCopyExistingShareLink(staleLocalLink, {
      creatingShareId: null,
      setCreatingShareId: (id) => creatingTransitions.push(id),
      fetchShareLinks: async () => {
        fetchShareLinksCalls += 1;
        return [
          {
            ...staleLocalLink,
            access_count: 1,
            max_views: 1,
          },
        ];
      },
      shareUrlMap,
      setShareUrlMap: (updater) => {
        shareUrlMap =
          typeof updater === "function" ? updater(shareUrlMap) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        generatedShareUrl =
          typeof updater === "function" ? updater(generatedShareUrl) : updater;
      },
      copyUrlToClipboard: async (url) => {
        copiedUrls.push(url);
      },
      notify: (msg) => notifications.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async () => {
        fetchCalls += 1;
        throw new Error("fetchImpl must not be called");
      }) as unknown as typeof fetch,
    });

    expect(fetchShareLinksCalls).toBe(1);
    expect(fetchCalls).toBe(0);
    expect(copiedUrls).toEqual([]);
    expect(shareUrlMap).toEqual({
      shr_other: "https://nvr.example.test/share/tok_other.sig",
    });
    expect(generatedShareUrl).toBe("");
    expect(notifications).toEqual(["Share link is no longer active"]);
    expect(creatingTransitions).toEqual(["exp_1", null]);

    // 2. Server refresh confirms link is still active: copies cached knownUrl without rotating
    let activeUrlMap: Record<string, string> = {
      shr_cached_1: cachedUrl,
    };
    let activeGenUrl = "";
    const activeCopied: string[] = [];
    const activeNotes: string[] = [];
    let activeRefreshCalls = 0;

    await executeCopyExistingShareLink(staleLocalLink, {
      creatingShareId: null,
      fetchShareLinks: async () => {
        activeRefreshCalls += 1;
        return [staleLocalLink];
      },
      shareUrlMap: activeUrlMap,
      setShareUrlMap: (updater) => {
        activeUrlMap =
          typeof updater === "function" ? updater(activeUrlMap) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        activeGenUrl =
          typeof updater === "function" ? updater(activeGenUrl) : updater;
      },
      copyUrlToClipboard: async (url) => {
        activeCopied.push(url);
      },
      notify: (msg) => activeNotes.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async () => {
        throw new Error("fetchImpl must not be called when knownUrl is active");
      }) as unknown as typeof fetch,
    });

    expect(activeRefreshCalls).toBe(1);
    expect(activeCopied).toEqual([cachedUrl]);
    expect(activeUrlMap).toEqual({ shr_cached_1: cachedUrl });
    expect(activeNotes).toEqual([]);

    // 3. Corrupted or cross-origin entry in shareUrlMap is rejected by buildSharePublicUrl and not copied directly
    let corruptedUrlMap: Record<string, string> = {
      shr_cached_1: "https://evil.example.test/share/tok_cached_1.sig",
    };
    const corruptedCopied: string[] = [];
    await executeCopyExistingShareLink(
      { ...staleLocalLink, token: "tok_fallback_valid.sig" },
      {
        creatingShareId: null,
        fetchShareLinks: async () => [
          { ...staleLocalLink, token: "tok_fallback_valid.sig" },
        ],
        shareUrlMap: corruptedUrlMap,
        setShareUrlMap: (updater) => {
          corruptedUrlMap =
            typeof updater === "function" ? updater(corruptedUrlMap) : updater;
        },
        setGeneratedShareUrl: () => {},
        copyUrlToClipboard: async (url) => {
          corruptedCopied.push(url);
        },
        notify: () => {},
        origin: "https://nvr.example.test",
        fetchImpl: (async () => {
          throw new Error("fetchImpl must not be called when fallback token is valid");
        }) as unknown as typeof fetch,
      }
    );
    expect(corruptedCopied).toEqual([
      "https://nvr.example.test/share/tok_fallback_valid.sig",
    ]);
  });

  test("executeCopyExistingShareLink does not fall back to stale link when pruned from /api/share response", async () => {
    const futureExpires = Math.floor(Date.now() / 1000) + 3600;
    const prunedLink = makeShareLink({
      id: "shr_pruned_99",
      target_id: "exp_pruned",
      expires_at: futureExpires,
      revoked: false,
    });
    const prunedUrl = "https://nvr.example.test/share/tok_pruned.sig";

    let shareUrlMap: Record<string, string> = {
      shr_pruned_99: prunedUrl,
    };
    let generatedShareUrl = prunedUrl;
    const notifications: string[] = [];
    const copiedUrls: string[] = [];
    let apiCalls = 0;

    // Server returns a valid array that no longer contains shr_pruned_99
    await executeCopyExistingShareLink(prunedLink, {
      creatingShareId: null,
      fetchShareLinks: async () => [
        makeShareLink({ id: "shr_different_1", expires_at: futureExpires }),
      ],
      shareUrlMap,
      setShareUrlMap: (updater) => {
        shareUrlMap =
          typeof updater === "function" ? updater(shareUrlMap) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        generatedShareUrl =
          typeof updater === "function" ? updater(generatedShareUrl) : updater;
      },
      copyUrlToClipboard: async (url) => {
        copiedUrls.push(url);
      },
      notify: (msg) => notifications.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async () => {
        apiCalls += 1;
        throw new Error("must not rotate pruned link");
      }) as unknown as typeof fetch,
    });

    expect(apiCalls).toBe(0);
    expect(copiedUrls).toEqual([]);
    expect(shareUrlMap).toEqual({});
    expect(generatedShareUrl).toBe("");
    expect(notifications).toEqual(["Share link is no longer active"]);

    // Transient network error (fetchShareLinks returns null) falls back to link
    const fallbackCopied: string[] = [];
    await executeCopyExistingShareLink(prunedLink, {
      creatingShareId: null,
      fetchShareLinks: async () => null,
      shareUrlMap: { shr_pruned_99: prunedUrl },
      setShareUrlMap: () => {},
      setGeneratedShareUrl: () => {},
      copyUrlToClipboard: async (url) => {
        fallbackCopied.push(url);
      },
      notify: () => {},
      origin: "https://nvr.example.test",
    });
    expect(fallbackCopied).toEqual([prunedUrl]);
  });

  test("executeCopyExistingShareLink cleans up old link state immediately after DELETE and refreshes list if POST /api/share fails or throws", async () => {
    const futureExpires = Math.floor(Date.now() / 1000) + 7200;
    const rotLink = makeShareLink({
      id: "shr_rot_old",
      target_id: "exp_rot",
      expires_at: futureExpires,
      max_views: 5,
      access_count: 2,
      revoked: false,
    });
    const staleOldUrl = "https://nvr.example.test/share/old_rot.sig";

    // Case A: DELETE succeeds (200), then POST /api/share returns 500 (!res.ok)
    // State cleanup of shr_rot_old must happen BEFORE POST /api/share is called,
    // and fetchShareLinks() must be called after POST fails.
    let shareUrlMapA: Record<string, string> = {};
    let generatedShareUrlA = staleOldUrl;
    let mapSnapshotDuringPostA: Record<string, string> = {};
    let genUrlSnapshotDuringPostA = "__unset__";
    let refreshCountA = 0;
    const notificationsA: string[] = [];
    const requestsA: Array<{ url: string; method: string }> = [];

    await executeCopyExistingShareLink(rotLink, {
      creatingShareId: null,
      fetchShareLinks: async () => {
        refreshCountA += 1;
        return [rotLink];
      },
      shareUrlMap: shareUrlMapA,
      setShareUrlMap: (updater) => {
        shareUrlMapA =
          typeof updater === "function" ? updater(shareUrlMapA) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        generatedShareUrlA =
          typeof updater === "function"
            ? updater(generatedShareUrlA)
            : updater;
      },
      copyUrlToClipboard: async () => {},
      notify: (msg) => notificationsA.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method || "GET";
        requestsA.push({ url, method });
        if (method === "DELETE") {
          // Simulate concurrent map entry populated prior to DELETE completion
          shareUrlMapA = {
            shr_rot_old: staleOldUrl,
            shr_keep: "https://nvr.example.test/share/keep.sig",
          };
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (method === "POST") {
          mapSnapshotDuringPostA = { ...shareUrlMapA };
          genUrlSnapshotDuringPostA = generatedShareUrlA;
          return new Response(
            JSON.stringify({ message: "Minting rate limit exceeded" }),
            { status: 429 }
          );
        }
        throw new Error(`Unexpected request: ${method} ${url}`);
      }) as unknown as typeof fetch,
    });

    expect(requestsA).toEqual([
      { url: "/api/share/shr_rot_old", method: "DELETE" },
      { url: "/api/share", method: "POST" },
    ]);
    // Verified BEFORE POST resolved: shr_rot_old was already removed and generatedShareUrl was cleared
    expect(mapSnapshotDuringPostA).toEqual({
      shr_keep: "https://nvr.example.test/share/keep.sig",
    });
    expect(genUrlSnapshotDuringPostA).toBe("");
    expect(refreshCountA).toBe(2);
    expect(notificationsA).toEqual(["Minting rate limit exceeded"]);

    // Case B: DELETE succeeds (404 already gone), then POST /api/share throws network error
    // catch block must call fetchShareLinks() so UI reflects revoked old link
    let shareUrlMapB: Record<string, string> = {};
    let generatedShareUrlB = staleOldUrl;
    let refreshCountB = 0;
    const notificationsB: string[] = [];
    const creatingTransitionsB: Array<string | null> = [];

    await executeCopyExistingShareLink(rotLink, {
      creatingShareId: null,
      setCreatingShareId: (id) => creatingTransitionsB.push(id),
      fetchShareLinks: async () => {
        refreshCountB += 1;
        return [rotLink];
      },
      shareUrlMap: shareUrlMapB,
      setShareUrlMap: (updater) => {
        shareUrlMapB =
          typeof updater === "function" ? updater(shareUrlMapB) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        generatedShareUrlB =
          typeof updater === "function"
            ? updater(generatedShareUrlB)
            : updater;
      },
      copyUrlToClipboard: async () => {},
      notify: (msg) => notificationsB.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = init?.method || "GET";
        if (method === "DELETE") {
          shareUrlMapB = { shr_rot_old: staleOldUrl };
          return new Response(null, { status: 404 });
        }
        throw new Error("Simulated POST network disconnect");
      }) as unknown as typeof fetch,
    });

    expect(shareUrlMapB).toEqual({});
    expect(generatedShareUrlB).toBe("");
    expect(refreshCountB).toBe(2);
    expect(notificationsB).toEqual(["Failed to generate share link"]);
    expect(creatingTransitionsB).toEqual(["exp_rot", null]);

    // Case C: Full rotation success (DELETE 200 + POST 200)
    let shareUrlMapC: Record<string, string> = {};
    let generatedShareUrlC = "";
    let refreshCountC = 0;
    const copiedC: Array<{ url: string; ttl?: string }> = [];
    let postBodyC: Record<string, unknown> = {};

    await executeCopyExistingShareLink(rotLink, {
      creatingShareId: null,
      fetchShareLinks: async () => {
        refreshCountC += 1;
        return [rotLink];
      },
      shareUrlMap: shareUrlMapC,
      setShareUrlMap: (updater) => {
        shareUrlMapC =
          typeof updater === "function" ? updater(shareUrlMapC) : updater;
      },
      setGeneratedShareUrl: (updater) => {
        generatedShareUrlC =
          typeof updater === "function"
            ? updater(generatedShareUrlC)
            : updater;
      },
      copyUrlToClipboard: async (url, ttl) => {
        generatedShareUrlC = url;
        copiedC.push({ url, ttl });
      },
      notify: () => {},
      origin: "https://nvr.example.test",
      fetchImpl: (async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = init?.method || "GET";
        if (method === "DELETE") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        postBodyC = JSON.parse(String(init?.body || "{}"));
        return new Response(
          JSON.stringify({
            id: "shr_rot_new",
            url: "/share/new_rot.sig",
          }),
          { status: 200 }
        );
      }) as unknown as typeof fetch,
    });

    expect(postBodyC?.max_views).toBe(3);
    expect(shareUrlMapC).toEqual({
      shr_rot_new: "https://nvr.example.test/share/new_rot.sig",
    });
    expect(generatedShareUrlC).toBe(
      "https://nvr.example.test/share/new_rot.sig"
    );
    expect(refreshCountC).toBe(2);
    expect(copiedC.length).toBe(1);
    expect(copiedC[0].url).toBe("https://nvr.example.test/share/new_rot.sig");

    // Case D: POST 200 returns an unsafe / cross-origin URL rejected by buildSharePublicUrl
    let shareUrlMapD: Record<string, string> = {};
    let refreshCountD = 0;
    const copiedD: string[] = [];
    const notificationsD: string[] = [];

    await executeCopyExistingShareLink(rotLink, {
      creatingShareId: null,
      fetchShareLinks: async () => {
        refreshCountD += 1;
        return [rotLink];
      },
      shareUrlMap: shareUrlMapD,
      setShareUrlMap: (updater) => {
        shareUrlMapD =
          typeof updater === "function" ? updater(shareUrlMapD) : updater;
      },
      setGeneratedShareUrl: () => {},
      copyUrlToClipboard: async (url) => {
        copiedD.push(url);
      },
      notify: (msg) => notificationsD.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = init?.method || "GET";
        if (method === "DELETE") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        return new Response(
          JSON.stringify({
            id: "shr_rot_evil",
            url: "https://evil.example/share/phish.sig",
          }),
          { status: 200 }
        );
      }) as unknown as typeof fetch,
    });

    expect(shareUrlMapD).toEqual({});
    expect(copiedD).toEqual([]);
    expect(refreshCountD).toBe(2);
    expect(notificationsD).toEqual(["Failed to generate share link"]);

    // Case E: Rotating a link with < 30 minutes remaining formats TTL with formatShareLinkRemainingTtl (never "0h")
    const shortTtlLink = makeShareLink({
      id: "shr_rot_short",
      target_id: "exp_short",
      expires_at: Math.floor(Date.now() / 1000) + 900,
      revoked: false,
    });
    const copiedShort: Array<{ url: string; ttl?: string }> = [];

    await executeCopyExistingShareLink(shortTtlLink, {
      creatingShareId: null,
      fetchShareLinks: async () => [shortTtlLink],
      shareUrlMap: {},
      setShareUrlMap: () => {},
      setGeneratedShareUrl: () => {},
      copyUrlToClipboard: async (url, ttl) => {
        copiedShort.push({ url, ttl });
      },
      notify: () => {},
      origin: "https://nvr.example.test",
      fetchImpl: (async (_input: RequestInfo | URL, init?: RequestInit) => {
        const method = init?.method || "GET";
        if (method === "DELETE") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        return new Response(
          JSON.stringify({
            id: "shr_rot_short_new",
            url: "/share/short_rot.sig",
          }),
          { status: 200 }
        );
      }) as unknown as typeof fetch,
    });

    expect(copiedShort.length).toBe(1);
    expect(copiedShort[0].ttl).not.toBe("0h");
    expect(copiedShort[0].ttl).toMatch(/^(14m \d+s|15m)$/);
  });

  test("executeCopyExistingShareLink returns immediately without calling fetchShareLinks, DELETE, or POST when revokingShareId or creatingShareId is non-null", async () => {
    const futureExpires = Math.floor(Date.now() / 1000) + 3600;
    const link = makeShareLink({
      id: "shr_concurrent_1",
      target_id: "exp_concurrent_1",
      expires_at: futureExpires,
      revoked: false,
    });

    let fetchShareLinksCalls = 0;
    let fetchCalls = 0;
    const creatingTransitions: Array<string | null> = [];
    const copiedUrls: string[] = [];
    const notifications: string[] = [];

    // 1. Concurrent revocation in flight (revokingShareId is non-null)
    await executeCopyExistingShareLink(link, {
      creatingShareId: null,
      revokingShareId: "shr_concurrent_1",
      setCreatingShareId: (id) => creatingTransitions.push(id),
      fetchShareLinks: async () => {
        fetchShareLinksCalls += 1;
        return [link];
      },
      shareUrlMap: {},
      setShareUrlMap: () => {},
      setGeneratedShareUrl: () => {},
      copyUrlToClipboard: async (url) => {
        copiedUrls.push(url);
      },
      notify: (msg) => notifications.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async () => {
        fetchCalls += 1;
        throw new Error("fetchImpl must not be called when revokingShareId is non-null");
      }) as unknown as typeof fetch,
    });

    expect(fetchShareLinksCalls).toBe(0);
    expect(fetchCalls).toBe(0);
    expect(creatingTransitions).toEqual([]);
    expect(copiedUrls).toEqual([]);
    expect(notifications).toEqual([]);

    // 2. Concurrent creation/rotation in flight (creatingShareId is non-null)
    await executeCopyExistingShareLink(link, {
      creatingShareId: "exp_concurrent_1",
      revokingShareId: null,
      setCreatingShareId: (id) => creatingTransitions.push(id),
      fetchShareLinks: async () => {
        fetchShareLinksCalls += 1;
        return [link];
      },
      shareUrlMap: {},
      setShareUrlMap: () => {},
      setGeneratedShareUrl: () => {},
      copyUrlToClipboard: async (url) => {
        copiedUrls.push(url);
      },
      notify: (msg) => notifications.push(msg),
      origin: "https://nvr.example.test",
      fetchImpl: (async () => {
        fetchCalls += 1;
        throw new Error("fetchImpl must not be called when creatingShareId is non-null");
      }) as unknown as typeof fetch,
    });

    expect(fetchShareLinksCalls).toBe(0);
    expect(fetchCalls).toBe(0);
    expect(creatingTransitions).toEqual([]);
    expect(copiedUrls).toEqual([]);
    expect(notifications).toEqual([]);
  });
});
