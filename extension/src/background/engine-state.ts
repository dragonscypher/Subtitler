import type { EngineConnectionState, NativeLocalProcessingAdvisory } from "../shared/protocol";

/**
 * In-memory state for one native-engine connection. It deliberately does not
 * use chrome.storage: a resumed service worker must receive a new handshake
 * before presenting a local-processing recommendation.
 */
export class EngineConnectionStateStore {
  private state: EngineConnectionState = { connected: false, localProcessingAvailable: false };

  markReady(localProcessingAvailable: boolean, advisory: NativeLocalProcessingAdvisory | undefined): EngineConnectionState {
    const next: EngineConnectionState = { connected: true, localProcessingAvailable };
    if (advisory) {
      next.localProcessingAdvisory = { ...advisory };
    }
    this.state = next;
    return this.snapshot();
  }

  markDisconnected(): EngineConnectionState {
    this.state = { connected: false, localProcessingAvailable: false };
    return this.snapshot();
  }

  snapshot(): EngineConnectionState {
    const snapshot: EngineConnectionState = {
      connected: this.state.connected,
      localProcessingAvailable: this.state.localProcessingAvailable
    };
    if (this.state.localProcessingAdvisory) {
      snapshot.localProcessingAdvisory = { ...this.state.localProcessingAdvisory };
    }
    return snapshot;
  }
}
