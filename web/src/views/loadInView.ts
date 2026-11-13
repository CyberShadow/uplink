// Loading more as the end of what is loaded comes into view.

import type { RefObject } from 'preact'
import { useEffect, useRef } from 'preact/hooks'

/** Calls `load` once the element comes into view (in the window, and
 * in its scroller), and again whenever `key` changes while it is in
 * view: a load that brought something changes it, so that loading goes
 * on until the view is full; one that failed is not repeated. */
export function useLoadInView(element: RefObject<HTMLElement>, key: unknown, load: () => void) {
  const current = useRef(load)
  current.current = load
  useEffect(() => {
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) return
      observer.disconnect()
      current.current()
    })
    observer.observe(element.current!)
    return () => observer.disconnect()
  }, [element, key])
}
