// WebRTC peer connection (main thread: workers cannot create one), offered
// over the stream WebSocket once the server sends its config. ICE finds a
// direct UDP path when one exists (LAN, tailnet, punched NAT); the worker
// then compares round trips and moves frames and input over when that path
// is shorter than the WebSocket's. Without it everything keeps using the
// WebSocket.

import { now } from './proto';

export interface PeerEvents {
  signal(json: object): void;
  /** A frames-channel message (frame chunk or pong). */
  message(buf: ArrayBuffer, received: number): void;
  /** The frames channel opened or went away. */
  open(open: boolean): void;
}

export class Peer {
  private pc: RTCPeerConnection | null = null;
  private frames: RTCDataChannel | null = null;
  private input: RTCDataChannel | null = null;

  constructor(private readonly events: PeerEvents) {}

  async start(iceServers: RTCIceServer[]): Promise<void> {
    this.close();
    const pc = new RTCPeerConnection({ iceServers });
    this.pc = pc;
    // Reliable but unordered: no frame waits behind another's retransmission.
    // (Unreliable delivery would not help: SCTP congestion control treats
    // abandoned packets as losses all the same.)
    const frames = pc.createDataChannel('frames', { ordered: false });
    frames.binaryType = 'arraybuffer';
    frames.onmessage = (e) => this.events.message(e.data as ArrayBuffer, now());
    frames.onopen = () => this.events.open(true);
    frames.onclose = () => this.events.open(false);
    this.frames = frames;
    this.input = pc.createDataChannel('input', { ordered: true });
    pc.onicecandidate = (e) => {
      if (e.candidate?.candidate) this.events.signal({ type: 'candidate', candidate: e.candidate.candidate });
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed' || pc.connectionState === 'closed') this.close();
    };
    await pc.setLocalDescription(await pc.createOffer());
    this.events.signal({ type: 'offer', sdp: pc.localDescription!.sdp });
  }

  async answer(sdp: string): Promise<void> {
    await this.pc?.setRemoteDescription({ type: 'answer', sdp });
  }

  sendFrames(buf: ArrayBuffer): void {
    if (this.frames?.readyState === 'open') this.frames.send(buf);
  }

  /** False when the input channel is not open (the caller uses the WebSocket). */
  sendInput(m: Uint8Array): boolean {
    if (this.input?.readyState !== 'open') return false;
    this.input.send(m as Uint8Array<ArrayBuffer>);
    return true;
  }

  close(): void {
    if (!this.pc) return;
    this.pc.close();
    this.pc = this.frames = this.input = null;
    this.events.open(false);
  }
}
