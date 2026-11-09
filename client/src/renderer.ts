// WebGL2 compositor (runs in the render worker): one screen-sized texture
// holding the exact server framebuffer (BGRX bytes uploaded as RGBA and
// swizzled in the shader).

const VS = `#version 300 es
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const FS = `#version 300 es
precision highp float;
uniform highp sampler2D u_screen;
uniform int u_height;
out vec4 o;
void main() {
  ivec2 p = ivec2(gl_FragCoord.xy);
  p.y = u_height - 1 - p.y;
  o = vec4(texelFetch(u_screen, p, 0).bgr, 1.0);
}`;

export class Renderer {
  readonly gl: WebGL2RenderingContext;
  private tex: WebGLTexture | null = null;
  private uHeight: WebGLUniformLocation;
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
    gl.uniform1i(gl.getUniformLocation(prog, 'u_screen'), 0);
    this.uHeight = gl.getUniformLocation(prog, 'u_height')!;
    gl.bindVertexArray(gl.createVertexArray());
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
  }

  resize(width: number, height: number): void {
    const gl = this.gl;
    this.width = width;
    this.height = height;
    this.canvas.width = width;
    this.canvas.height = height;
    if (this.tex) gl.deleteTexture(this.tex);
    this.tex = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, width, height);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.viewport(0, 0, width, height);
    gl.uniform1i(this.uHeight, height);
  }

  upload(x: number, y: number, w: number, h: number, bgrx: Uint8Array): void {
    this.gl.texSubImage2D(this.gl.TEXTURE_2D, 0, x, y, w, h, this.gl.RGBA, this.gl.UNSIGNED_BYTE, bgrx);
  }

  draw(): void {
    this.gl.drawArrays(this.gl.TRIANGLES, 0, 3);
  }

  /** Reads `width` pixels of screen row `y` (top-down) from the last draw. */
  readRow(y: number, width: number, out: Uint8Array): void {
    this.gl.readPixels(0, this.height - 1 - y, width, 1, this.gl.RGBA, this.gl.UNSIGNED_BYTE, out);
  }
}
