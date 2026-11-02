export type Rect = { left: number; top: number; width: number; height: number };

/** Converts viewport coordinates into the normalized coordinates of a contain-fit video. */
export function normalizedPoint(clientX: number, clientY: number, bounds: Rect, videoWidth: number, videoHeight: number) {
  if (bounds.width <= 0 || bounds.height <= 0 || videoWidth <= 0 || videoHeight <= 0) return null;
  const scale = Math.min(bounds.width / videoWidth, bounds.height / videoHeight);
  const width = videoWidth * scale;
  const height = videoHeight * scale;
  const left = bounds.left + (bounds.width - width) / 2;
  const top = bounds.top + (bounds.height - height) / 2;
  const x = (clientX - left) / width;
  const y = (clientY - top) / height;
  if (!Number.isFinite(x) || !Number.isFinite(y) || x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}
