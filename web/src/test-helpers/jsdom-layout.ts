// jsdom returns 0x0 for getBoundingClientRect and 0 sizes through
// ResizeObserver. @tanstack/react-virtual uses both to compute the
// visible window, so without a polyfill virtualised lists collapse to
// "0 visible items" in tests. Patch both APIs to claim a plausible
// 1280x800 viewport so virtualisers render their full estimated range
// (which, in practice, is "everything" for any list small enough to fit
// in a test fixture).
//
// Loaded automatically via vitest.config.ts `setupFiles`.

const VIEWPORT_WIDTH = 1280;
const VIEWPORT_HEIGHT = 800;

if (typeof window !== "undefined") {
  // ResizeObserver in jsdom 29 is a no-op stub; replace with one that
  // immediately invokes the callback with a plausible content rect.
  class StubResizeObserver {
    private callback: ResizeObserverCallback;
    constructor(callback: ResizeObserverCallback) {
      this.callback = callback;
    }
    observe(target: Element) {
      const rect = {
        width: VIEWPORT_WIDTH,
        height: VIEWPORT_HEIGHT,
        top: 0,
        left: 0,
        bottom: VIEWPORT_HEIGHT,
        right: VIEWPORT_WIDTH,
        x: 0,
        y: 0,
        toJSON() {
          return this;
        },
      } as DOMRectReadOnly;
      const entry = {
        target,
        contentRect: rect,
        borderBoxSize: [{ inlineSize: rect.width, blockSize: rect.height }],
        contentBoxSize: [{ inlineSize: rect.width, blockSize: rect.height }],
        devicePixelContentBoxSize: [
          { inlineSize: rect.width, blockSize: rect.height },
        ],
      } as unknown as ResizeObserverEntry;
      // Fire async like the real API to give React a tick to commit.
      queueMicrotask(() => this.callback([entry], this as unknown as ResizeObserver));
    }
    unobserve() {}
    disconnect() {}
  }
  (
    window as unknown as { ResizeObserver: typeof ResizeObserver }
  ).ResizeObserver = StubResizeObserver as unknown as typeof ResizeObserver;

  // getBoundingClientRect on every Element returns all zeros under jsdom;
  // override to advertise the viewport so virtualisers can compute ranges.
  const originalGetBCR = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const r = originalGetBCR.call(this);
    if (r.width === 0 && r.height === 0) {
      return {
        width: VIEWPORT_WIDTH,
        height: VIEWPORT_HEIGHT,
        top: 0,
        left: 0,
        bottom: VIEWPORT_HEIGHT,
        right: VIEWPORT_WIDTH,
        x: 0,
        y: 0,
        toJSON() {
          return this;
        },
      } as DOMRect;
    }
    return r;
  };
}

export {};
