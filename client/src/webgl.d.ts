// WebGL drawing buffer storage (Chromium 122+), not yet in TypeScript's DOM
// library: picks the drawing buffer's format, RGBA16F for an HDR picture.
interface WebGL2RenderingContextBase {
  drawingBufferStorage(sizedFormat: GLenum, width: GLsizei, height: GLsizei): void;
}
