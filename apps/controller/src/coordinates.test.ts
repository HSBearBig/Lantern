import { describe, expect, it } from 'vitest';
import { normalizedPoint } from './coordinates';

describe('normalizedPoint', () => {
  it('maps through letterboxing instead of the entire container', () => {
    const rect = { left: 0, top: 0, width: 1000, height: 1000 };
    expect(normalizedPoint(500, 500, rect, 1920, 1080)).toEqual({ x: 0.5, y: 0.5 });
    expect(normalizedPoint(500, 10, rect, 1920, 1080)).toBeNull();
    expect(normalizedPoint(500, 218.75, rect, 1920, 1080)).toEqual({ x: 0.5, y: 0 });
  });

  it('rejects invalid dimensions and outside points', () => {
    expect(normalizedPoint(0, 0, { left: 0, top: 0, width: 0, height: 1 }, 1, 1)).toBeNull();
    expect(normalizedPoint(-1, 50, { left: 0, top: 0, width: 100, height: 100 }, 100, 100)).toBeNull();
  });
});
