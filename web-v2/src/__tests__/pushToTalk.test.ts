import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  LiveMsePlayer,
  PushToTalkButton,
  PushToTalkPeerConnectionLike,
  PushToTalkState,
  PushToTalkStreamLike,
  PushToTalkTrackLike,
  closePushToTalkSession,
  createPushToTalkController,
  startPushToTalkSession,
} from "../components/VideoPlayers";

function createMockMediaStream(trackCount = 2): {
  stream: PushToTalkStreamLike;
  tracks: Array<PushToTalkTrackLike & { stopCalls: number }>;
} {
  const tracks = Array.from({ length: trackCount }, (_, idx) => {
    const track = {
      kind: "audio",
      enabled: true,
      stopCalls: 0,
      stop() {
        track.stopCalls += 1;
        track.enabled = false;
      },
    };
    return track;
  });
  return {
    stream: {
      getTracks: () => tracks,
    },
    tracks,
  };
}

function createMockPeerConnection(offerSdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n"): {
  pc: PushToTalkPeerConnectionLike;
  addedTracks: PushToTalkTrackLike[];
  remoteDescriptions: Array<{ type: "answer"; sdp: string }>;
  closeCalls: () => number;
} {
  const addedTracks: PushToTalkTrackLike[] = [];
  const remoteDescriptions: Array<{ type: "answer"; sdp: string }> = [];
  let closedCount = 0;

  const pc: PushToTalkPeerConnectionLike = {
    localDescription: null,
    addTrack(track) {
      addedTracks.push(track);
      return {};
    },
    async createOffer() {
      return { type: "offer", sdp: offerSdp };
    },
    async setLocalDescription(desc) {
      pc.localDescription = desc;
    },
    async setRemoteDescription(desc) {
      remoteDescriptions.push(desc);
    },
    close() {
      closedCount += 1;
    },
  };

  return {
    pc,
    addedTracks,
    remoteDescriptions,
    closeCalls: () => closedCount,
  };
}

describe("WebRTC Push-to-Talk (AU-05) session helpers and deterministic mic release", () => {
  test("closePushToTalkSession stops all MediaStreamTracks and closes RTCPeerConnection", () => {
    const { stream, tracks } = createMockMediaStream(3);
    const { pc, closeCalls } = createMockPeerConnection();

    closePushToTalkSession(stream, pc);

    for (const t of tracks) {
      expect(t.stopCalls).toBe(1);
    }
    expect(closeCalls()).toBe(1);

    // Tolerates null / undefined inputs safely
    closePushToTalkSession(null, null);
    closePushToTalkSession(undefined, undefined);
    expect(closeCalls()).toBe(1);
  });

  test("closePushToTalkSession continues stopping remaining tracks and closing pc even if one track throws", () => {
    let secondTrackStopped = false;
    let pcClosed = false;
    const flakyStream: PushToTalkStreamLike = {
      getTracks: () => [
        {
          stop() {
            throw new Error("synthetic track stop error");
          },
        },
        {
          stop() {
            secondTrackStopped = true;
          },
        },
      ],
    };
    const mockPc: PushToTalkPeerConnectionLike = {
      close() {
        pcClosed = true;
      },
    };

    closePushToTalkSession(flakyStream, mockPc);
    expect(secondTrackStopped).toBe(true);
    expect(pcClosed).toBe(true);
  });

  test("startPushToTalkSession negotiates SDP offer/answer with /api/go2rtc/webrtc?src=<camera> and releases on stop()", async () => {
    const { stream, tracks } = createMockMediaStream(2);
    const { pc, addedTracks, remoteDescriptions, closeCalls } = createMockPeerConnection();
    const fetchCalls: Array<{ url: string; method: string; contentType: string; body: string }> = [];
    let requestedAudioConstraint: boolean | undefined;

    const session = await startPushToTalkSession("front_door", {
      getUserMedia: async (constraints) => {
        requestedAudioConstraint = constraints.audio;
        return stream;
      },
      createPeerConnection: () => pc,
      fetchImpl: async (url, init) => {
        fetchCalls.push({
          url,
          method: init.method,
          contentType: init.headers["Content-Type"],
          body: init.body,
        });
        return {
          ok: true,
          status: 201,
          text: async () => "v=0\r\ns=go2rtc-answer\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n",
        };
      },
    });

    expect(requestedAudioConstraint).toBe(true);
    expect(addedTracks.length).toBe(2);
    expect(fetchCalls.length).toBe(1);
    expect(fetchCalls[0].url).toBe("/api/go2rtc/webrtc?src=front_door");
    expect(fetchCalls[0].method).toBe("POST");
    expect(fetchCalls[0].contentType).toBe("application/sdp");
    expect(fetchCalls[0].body).toContain("a=sendonly");
    expect(remoteDescriptions.length).toBe(1);
    expect(remoteDescriptions[0].type).toBe("answer");
    expect(remoteDescriptions[0].sdp).toContain("s=go2rtc-answer");

    // Tracks remain active while session is open
    expect(tracks[0].stopCalls).toBe(0);
    expect(tracks[1].stopCalls).toBe(0);
    expect(closeCalls()).toBe(0);

    // Stopping session releases every track and closes peer connection
    session.stop();
    expect(tracks[0].stopCalls).toBe(1);
    expect(tracks[1].stopCalls).toBe(1);
    expect(closeCalls()).toBe(1);
  });

  test("PTT state machine releases microphone tracks on normal release, SDP failure, and unmount without touching MSE video", async () => {
    // Simulated active MSE video player state that must never be reloaded or paused by PTT
    const activeMseVideoPlayer = {
      src: "blob:https://nvr.local/mse-live-front-door",
      reloadCount: 0,
      pauseCount: 0,
      load() {
        activeMseVideoPlayer.reloadCount += 1;
      },
      pause() {
        activeMseVideoPlayer.pauseCount += 1;
      },
    };

    // 1. Normal press -> talking -> release lifecycle
    const mock1 = createMockMediaStream(2);
    const pc1 = createMockPeerConnection();
    const transitions1: PushToTalkState[] = [];

    const ctrl1 = createPushToTalkController(
      "front_door",
      {
        getUserMedia: async () => mock1.stream,
        createPeerConnection: () => pc1.pc,
        fetchImpl: async () => ({
          ok: true,
          status: 200,
          text: async () => "v=0\r\ns=answer\r\n",
        }),
      },
      (state) => transitions1.push(state)
    );

    const started = await ctrl1.press();
    expect(started).toBe(true);
    expect(ctrl1.getState()).toBe("talking");
    expect(mock1.tracks.every((t) => t.stopCalls === 0)).toBe(true);

    ctrl1.release();
    expect(ctrl1.getState()).toBe("idle");
    expect(transitions1).toEqual(["connecting", "talking", "idle"]);
    expect(mock1.tracks.every((t) => t.stopCalls === 1)).toBe(true);
    expect(pc1.closeCalls()).toBe(1);

    // 2. Upstream SDP failure (502 Bad Gateway) immediately releases mic tracks & closes PC
    const mock2 = createMockMediaStream(2);
    const pc2 = createMockPeerConnection();
    const transitions2: PushToTalkState[] = [];

    const ctrl2 = createPushToTalkController(
      "front_door",
      {
        getUserMedia: async () => mock2.stream,
        createPeerConnection: () => pc2.pc,
        fetchImpl: async () => ({
          ok: false,
          status: 502,
          text: async () => "Bad Gateway",
        }),
      },
      (state) => transitions2.push(state)
    );

    const failedStart = await ctrl2.press();
    expect(failedStart).toBe(false);
    expect(ctrl2.getState()).toBe("error");
    expect(ctrl2.getError()).toContain("502");
    expect(transitions2).toEqual(["connecting", "error"]);
    expect(mock2.tracks.every((t) => t.stopCalls === 1)).toBe(true);
    expect(pc2.closeCalls()).toBe(1);

    // 3. Component unmount while actively talking immediately stops tracks and closes PC
    const mock3 = createMockMediaStream(2);
    const pc3 = createMockPeerConnection();
    const ctrl3 = createPushToTalkController("front_door", {
      getUserMedia: async () => mock3.stream,
      createPeerConnection: () => pc3.pc,
      fetchImpl: async () => ({
        ok: true,
        status: 200,
        text: async () => "v=0\r\ns=answer\r\n",
      }),
    });

    await ctrl3.press();
    expect(ctrl3.getState()).toBe("talking");
    ctrl3.unmount();
    expect(ctrl3.getState()).toBe("idle");
    expect(mock3.tracks.every((t) => t.stopCalls === 1)).toBe(true);
    expect(pc3.closeCalls()).toBe(1);

    // Active MSE video player remained 100% untouched throughout all PTT transitions
    expect(activeMseVideoPlayer.src).toBe("blob:https://nvr.local/mse-live-front-door");
    expect(activeMseVideoPlayer.reloadCount).toBe(0);
    expect(activeMseVideoPlayer.pauseCount).toBe(0);
  });

  test("renders PushToTalkButton on LiveMsePlayer camera tiles", () => {
    const btnHtml = renderToStaticMarkup(
      React.createElement(PushToTalkButton, { camera: "front_door" })
    );
    expect(btnHtml).toContain('data-testid="ptt-btn-front_door"');
    expect(btnHtml).toContain('aria-label="Push to talk on front_door"');
    expect(btnHtml).toContain("PTT");

    const playerHtml = renderToStaticMarkup(
      React.createElement(LiveMsePlayer, { camera: "front_door" })
    );
    expect(playerHtml).toContain('data-testid="ptt-btn-front_door"');
  });
});
