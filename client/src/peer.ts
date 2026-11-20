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
  /** Something for the server's event log (what ICE offered, how it ended). */
  log(text: string): void;
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
    // What ICE offers says whether this browser may use UDP at all (a
    // policy can hide or forbid it): reported once gathering ends.
    const offered: Record<string, number> = {};
    pc.onicecandidate = (e) => {
      const c = e.candidate;
      if (c?.candidate) {
        this.events.signal({ type: 'candidate', candidate: c.candidate });
        const kind = `${c.type ?? '?'} ${c.protocol ?? '?'}${c.address?.endsWith('.local') ? ' mDNS' : ''}`;
        offered[kind] = (offered[kind] ?? 0) + 1;
      } else if (!c) {
        const list = Object.entries(offered).map(([kind, n]) => `${n} ${kind}`).join(', ');
        this.events.log(`webrtc: candidates offered: ${list || 'none'}`);
      }
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'connected' || pc.connectionState === 'failed') this.events.log(`webrtc: ${pc.connectionState}`);
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
