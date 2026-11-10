// WebGL2 compositor (runs in the stream worker). Two layers:
//  - lossless: exact server framebuffer (BGRX bytes uploaded as RGBA, swizzled here)
//  - video: last decoded H.264 frame of the whole screen
// A per-tile mask picks the layer for each tile.

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
out vec4 o;
void main() {
  ivec2 p = ivec2(gl_FragCoord.xy);
  p.y = u_height - 1 - p.y;
  if (texelFetch(u_mask, p / u_tile, 0).r > 0.5) {
    o = vec4(texelFetch(u_video, p, 0).rgb, 1.0);
  } else {
    o = vec4(texelFetch(u_screen, p, 0).bgr, 1.0);
  }
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
  private tilesX = 0;
  private tilesY = 0;
  width = 0;
  height = 0;

  constructor(readonly canvas: OffscreenCanvas) {
    const gl = canvas.getContext('webgl2', {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      desynchronized: true,
      preserveDrawingBuffer: false,
      powerPreference: 'high-performance',
    });
    if (!gl) throw new Error('WebGL2 unavailable');
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

  resize(width: number, height: number, tile: number): void {
    const gl = this.gl;
    this.width = width;
    this.height = height;
    this.canvas.width = width;
    this.canvas.height = height;
    for (const t of this.textures) gl.deleteTexture(t);
    gl.deleteFramebuffer(this.fbScreen);
    gl.deleteFramebuffer(this.fbScratch);
    this.tilesX = Math.ceil(width / tile);
    this.tilesY = Math.ceil(height / tile);
    this.mask = new Uint8Array(this.tilesX * this.tilesY);
    this.tiles = this.mask.length;
    this.videoTiles = 0;
    const [screen, video, mask, scratch] = [texture(gl, 0), texture(gl, 1), texture(gl, 2), texture(gl, 3)];
    this.textures = [screen, video, mask, scratch];
    gl.activeTexture(gl.TEXTURE3);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, width, height);
    this.fbScreen = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbScreen);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, screen, 0);
    this.fbScratch = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbScratch);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, scratch, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.activeTexture(gl.TEXTURE0);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, width, height);
    gl.activeTexture(gl.TEXTURE1);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, width, height);
    gl.activeTexture(gl.TEXTURE2);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, this.tilesX, this.tilesY);
    this.uploadMask();
    gl.viewport(0, 0, width, height);
    gl.uniform1i(gl.getUniformLocation(this.prog, 'u_height'), height);
    gl.uniform1i(gl.getUniformLocation(this.prog, 'u_tile'), tile);
  }

  upload(x: number, y: number, w: number, h: number, bgrx: Uint8Array): void {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGBA, gl.UNSIGNED_BYTE, bgrx);
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
    gl.activeTexture(gl.TEXTURE1);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, gl.RGBA, gl.UNSIGNED_BYTE, frame);
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
