import { describe, expect, test } from "bun:test";
import type { ShareLinkItem } from "../types/nvr";
import {
  buildSharePublicUrl,
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
});
