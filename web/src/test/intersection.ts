// jsdom does no layout, so nothing comes into view by itself: a test
// says what does, and it stays in view.

const observers = new Set<FakeIntersectionObserver>()
const visible = new WeakSet<Element>()

class FakeIntersectionObserver {
  private readonly targets = new Set<Element>()

  constructor(private readonly callback: IntersectionObserverCallback) {
    observers.add(this)
  }

  /** As a browser's, reports at first whether the target is in view;
   * here only if it is. */
  observe(target: Element) {
    this.targets.add(target)
    if (visible.has(target)) queueMicrotask(() => this.show(target))
  }

  unobserve(target: Element) {
    this.targets.delete(target)
  }

  disconnect() {
    this.targets.clear()
    observers.delete(this)
  }

  /** Reports `target` in view, if observed. */
  show(target: Element) {
    if (!this.targets.has(target)) return
    const entry = { target, isIntersecting: true, intersectionRatio: 1 } as IntersectionObserverEntry
    this.callback([entry], this as unknown as IntersectionObserver)
  }
}

window.IntersectionObserver = FakeIntersectionObserver as unknown as typeof IntersectionObserver

/** `target` comes into view, as scrolling would bring it, and stays. */
export function scrolledTo(target: Element) {
  visible.add(target)
  for (const observer of [...observers]) observer.show(target)
}
