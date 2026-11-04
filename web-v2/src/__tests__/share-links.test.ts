import { describe, expect, test } from "bun:test";
import type { ShareLinkItem } from "../types/nvr";
import {
  buildSharePublicUrl,
  computeShareLinkRotationParams,
  computeShareLinkStatus,
  filterActiveAndRecentShareLinks,
  formatShareLinkRemainingTtl,
  formatShareLinkViews,
} from "../utils/shareLinks";

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

  test("buildSharePublicUrl normalizes origins, relative paths, bare tokens, and full URLs", () => {
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
    expect(
      buildSharePublicUrl("https://nvr.example.test", "/custom/path")
    ).toBe("https://nvr.example.test/custom/path");
    expect(buildSharePublicUrl("", "/custom/path")).toBe("/custom/path");
    expect(buildSharePublicUrl("", "/share/tok.sig")).toBe("/share/tok.sig");
    expect(buildSharePublicUrl("", "tok.sig")).toBe("/share/tok.sig");
    expect(
      buildSharePublicUrl(
        "http://localhost:5000",
        "https://cdn.example.test/share/tok.sig"
      )
    ).toBe("https://cdn.example.test/share/tok.sig");
    expect(buildSharePublicUrl("http://localhost:5000", "   ")).toBe("");
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
});
