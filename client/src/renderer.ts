// WebGL2 compositor (runs in the stream worker). Two layers:
//  - lossless: exact server framebuffer (BGRX bytes uploaded as RGBA, swizzled
//    here; or, for an HDR picture, 10-bit R10G10B10A2 words)
//  - video: last decoded frame of the whole screen
// A per-tile mask picks the layer for each tile. An HDR picture (proto.rs
// S_HELLO) is decoded from its curve to linear light, 1.0 = SDR white, and
// drawn into a float drawing buffer as extended sRGB: what is brighter than
// white goes above 1, which the browser shows brighter when the page is in
// HDR (see `hdrSwitch` in main.ts).

const VS = `#version 300 es
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const FS = `#version 300 es
precision highp float;
uniform highp sampler2D u_screen;
uniform highp sampler2D u_video;
uniform highp sampler2D u_mask;
uniform int u_height;
uniform int u_tile;
uniform vec2 u_size;
// HDR picture: ln of its brightest value (multiples of SDR white) and the
// SDR range's share of the codes; u_split 0 for an SDR picture.
uniform float u_lnmax;
uniform float u_split;
out vec4 o;
vec3 decode(vec3 e) {
  vec3 s = e / u_split;
  vec3 sdr = mix(s / 12.92, pow((s + 0.055) / 1.055, vec3(2.4)), step(0.04045, s));
  vec3 high = exp((e - u_split) / (1.0 - u_split) * u_lnmax);
  return mix(sdr, high, step(u_split, e));
}
vec3 srgb(vec3 v) {
  return mix(12.92 * v, 1.055 * pow(v, vec3(1.0 / 2.4)) - 0.055, step(0.0031308, v));
}
void main() {
  ivec2 p = ivec2(gl_FragCoord.xy);
  p.y = u_height - 1 - p.y;
  vec3 c;
  if (texelFetch(u_mask, p / u_tile, 0).r > 0.5) {
    // The video may be encoded smaller than the screen (motion mode):
    // sampled at this pixel's centre, which at the same size is the texel itself.
    c = texture(u_video, (vec2(p) + 0.5) / u_size).rgb;
  } else {
    vec4 t = texelFetch(u_screen, p, 0);
    c = u_split > 0.0 ? t.rgb : t.bgr;
  }
  o = vec4(u_split > 0.0 ? srgb(decode(c)) : c, 1.0);
}`;

function texture(gl: WebGL2RenderingContext, unit: number): WebGLTexture {
  const t = gl.createTexture()!;
  gl.activeTexture(gl.TEXTURE0 + unit);
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  return t;
}

/** The SDR range's share of the codes in an HDR picture's curve (proto.rs `hdr_split`). */
export function hdrSplit(hdrMax: number): number {
  return 1 / (1 + (1.055 / 2.4) * Math.log(Math.max(hdrMax, 1)));
}

export class Renderer {
  readonly gl: WebGL2RenderingContext;
  private readonly prog: WebGLProgram;
  private textures: WebGLTexture[] = [];
  /** Framebuffers over the lossless texture and a scratch texture, for screen moves. */
  private fbScreen: WebGLFramebuffer | null = null;
  private fbScratch: WebGLFramebuffer | null = null;
  private mask = new Uint8Array(0);
  /** Tiles shown from the video layer, of `tiles` (for the stats overlay). */
  videoTiles = 0;
  tiles = 0;
  /** Size of the video texture: the encoded size of the last picture. */
  private videoWidth = 0;
  private videoHeight = 0;
  private tilesX = 0;
  private tilesY = 0;
  width = 0;
  height = 0;
  /** The picture is HDR: 10-bit tiles and video in the curve of proto.rs S_HELLO. */
  hdr = false;
  /** The drawing buffer is float, so what is brighter than white shows (HDR on screen). */
  float = false;
  /** This context can draw an HDR picture: a float drawing buffer (which an opaque canvas cannot have). */
  readonly hdrCapable: boolean;
  /** Words of an unaligned 10-bit upload. */
  private words = new Uint32Array(0);

  /** `hdr`: the page starts on an HDR screen, so the canvas gets alpha (a
   * float drawing buffer is refused on an opaque one); every pixel drawn is
   * opaque anyway. */
  constructor(readonly canvas: OffscreenCanvas, hdr: boolean) {
    const gl = canvas.getContext('webgl2', {
      alpha: hdr,
      antialias: false,
      depth: false,
      stencil: false,
      desynchronized: true,
      preserveDrawingBuffer: false,
      powerPreference: 'high-performance',
    });
    if (!gl) throw new Error('WebGL2 unavailable');
    this.hdrCapable = hdr && gl.getExtension('EXT_color_buffer_float') !== null && 'drawingBufferStorage' in gl;
    this.gl = gl;
    const prog = gl.createProgram()!;
    for (const [type, src] of [[gl.VERTEX_SHADER, VS], [gl.FRAGMENT_SHADER, FS]] as const) {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? 'shader');
      gl.attachShader(prog, s);
    }
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog) ?? 'link');
    gl.useProgram(prog);
    this.prog = prog;
    gl.uniform1i(gl.getUniformLocation(prog, 'u_screen'), 0);
    gl.uniform1i(gl.getUniformLocation(prog, 'u_video'), 1);
    gl.uniform1i(gl.getUniformLocation(prog, 'u_mask'), 2);
    gl.bindVertexArray(gl.createVertexArray());
  }

  /** `hdrMax` above 0: an HDR picture up to that many times SDR white. */
  resize(width: number, height: number, tile: number, hdrMax: number): void {
    const gl = this.gl;
    this.width = width;
    this.height = height;
    // A picture this page cannot show in HDR is still decoded, clipped at
    // white (a moment after it connects, until the server sends SDR).
    const hdr = hdrMax > 0;
    const float = hdr && this.hdrCapable;
    this.canvas.width = width;
    this.canvas.height = height;
    if (float || this.float) {
      gl.drawingBufferStorage(float ? gl.RGBA16F : gl.RGBA8, width, height);
    }
    this.hdr = hdr;
    this.float = float;
    const format = hdr ? gl.RGB10_A2 : gl.RGBA8;
    for (const t of this.textures) gl.deleteTexture(t);
    gl.deleteFramebuffer(this.fbScreen);
    gl.deleteFramebuffer(this.fbScratch);
    this.tilesX = Math.ceil(width / tile);
    this.tilesY = Math.ceil(height / tile);
    this.mask = new Uint8Array(this.tilesX * this.tilesY);
    this.tiles = this.mask.length;
    this.videoTiles = 0;
    // The video texture (unit 1) is made by `allocVideo` below.
    const [screen, mask, scratch] = [texture(gl, 0), texture(gl, 2), texture(gl, 3)];
    this.textures = [screen, gl.createTexture()!, mask, scratch];
    gl.activeTexture(gl.TEXTURE3);
    gl.texStorage2D(gl.TEXTURE_2D, 1, format, width, height);
    this.fbScreen = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbScreen);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, screen, 0);
    this.fbScratch = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbScratch);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, scratch, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.activeTexture(gl.TEXTURE0);
    gl.texStorage2D(gl.TEXTURE_2D, 1, format, width, height);
    this.allocVideo(width, height);
    gl.activeTexture(gl.TEXTURE2);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, this.tilesX, this.tilesY);
    this.uploadMask();
    gl.viewport(0, 0, width, height);
    gl.uniform1i(gl.getUniformLocation(this.prog, 'u_height'), height);
    gl.uniform2f(gl.getUniformLocation(this.prog, 'u_size'), width, height);
    gl.uniform1i(gl.getUniformLocation(this.prog, 'u_tile'), tile);
    gl.uniform1f(gl.getUniformLocation(this.prog, 'u_lnmax'), hdr ? Math.log(hdrMax) : 0);
    gl.uniform1f(gl.getUniformLocation(this.prog, 'u_split'), hdr ? hdrSplit(hdrMax) : 0);
  }

  upload(x: number, y: number, w: number, h: number, pixels: Uint8Array): void {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0);
    if (!this.hdr) {
      gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
      return;
    }
    // A u32 view needs 4-byte alignment; raw rects sit anywhere in their frame.
    let words: Uint32Array;
    if (pixels.byteOffset % 4 === 0) {
      words = new Uint32Array(pixels.buffer, pixels.byteOffset, pixels.byteLength / 4);
    } else {
      if (this.words.length < pixels.byteLength / 4) this.words = new Uint32Array(pixels.byteLength / 4);
      words = this.words.subarray(0, pixels.byteLength / 4);
      new Uint8Array(words.buffer, 0, pixels.byteLength).set(pixels);
    }
    gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGBA, gl.UNSIGNED_INT_2_10_10_10_REV, words);
  }

  /** Screen move on the lossless layer (through scratch: overlapping blits are undefined). */
  copy(sx: number, sy: number, dx: number, dy: number, w: number, h: number): void {
    const gl = this.gl;
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, this.fbScreen);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, this.fbScratch);
    gl.blitFramebuffer(sx, sy, sx + w, sy + h, 0, 0, w, h, gl.COLOR_BUFFER_BIT, gl.NEAREST);
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, this.fbScratch);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, this.fbScreen);
    gl.blitFramebuffer(0, 0, w, h, dx, dy, dx + w, dy + h, gl.COLOR_BUFFER_BIT, gl.NEAREST);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  }

  uploadVideo(frame: VideoFrame): void {
    const gl = this.gl;
    if (frame.displayWidth !== this.videoWidth || frame.displayHeight !== this.videoHeight) {
      this.allocVideo(frame.displayWidth, frame.displayHeight);
    }
    gl.activeTexture(gl.TEXTURE1);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, gl.RGBA, gl.UNSIGNED_BYTE, frame);
  }

  /** A video texture of the encoded size (immutable storage: a new one per size). */
  private allocVideo(width: number, height: number): void {
    const gl = this.gl;
    gl.deleteTexture(this.textures[1]);
    const t = texture(gl, 1);
    this.textures[1] = t;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, width, height);
    this.videoWidth = width;
    this.videoHeight = height;
  }

  /** Tile index with bit 15 set = show from video. */
  switchTile(entry: number): void {
    const tile = entry & 0x7fff;
    const video = entry & 0x8000 ? 255 : 0;
    if (video !== this.mask[tile]) this.videoTiles += video ? 1 : -1;
    this.mask[tile] = video;
  }

  uploadMask(): void {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE2);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, this.tilesX, this.tilesY, gl.RED, gl.UNSIGNED_BYTE, this.mask);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
  }

  draw(): void {
    this.gl.drawArrays(this.gl.TRIANGLES, 0, 3);
  }

  /** Reads `width` pixels of screen row `y` (top-down) from the last draw. */
  readRow(y: number, width: number, out: Uint8Array): void {
    this.gl.readPixels(0, this.height - 1 - y, width, 1, this.gl.RGBA, this.gl.UNSIGNED_BYTE, out);
  }
}
