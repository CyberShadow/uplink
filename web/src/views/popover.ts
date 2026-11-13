// Where a popover (a menu, a picker) opens: below its button, as it does
// by default, unless the box that clips it (the scroller it is in, as a
// timeline's, or the window) would cut it off there and there is more
// room above: then above it.

import type { RefObject } from 'preact'
import { useLayoutEffect, useState } from 'preact/hooks'

interface Span {
  top: number
  bottom: number
}

/** Whether a popover shown below its button (at `anchor`), where it
 * reaches `bottom`, opens above it instead, in `clip`. */
export function opensAbove(anchor: Span, bottom: number, clip: Span): boolean {
  return bottom > clip.bottom && anchor.top - clip.top > clip.bottom - anchor.bottom
}

/** Whether the popover, shown while `open`, opens above its button: its
 * parent, which it is positioned by. Measured as it opens, below. */
export function useOpensAbove(popover: RefObject<HTMLElement>, open: boolean): boolean {
  const [above, setAbove] = useState(false)
  useLayoutEffect(() => {
    if (!open) {
      setAbove(false)
      return
    }
    const element = popover.current!
    const anchor = element.parentElement!.getBoundingClientRect()
    setAbove(opensAbove(anchor, element.getBoundingClientRect().bottom, clipping(element)))
  }, [open, popover])
  return above
}

/** What of the window shows the element: its nearest ancestor that clips
 * what overflows it, within the window. */
function clipping(element: HTMLElement): Span {
  const window = { top: 0, bottom: document.documentElement.clientHeight }
  for (let box = element.parentElement; box; box = box.parentElement) {
    if (getComputedStyle(box).overflowY !== 'visible') {
      const rect = box.getBoundingClientRect()
      return { top: Math.max(rect.top, window.top), bottom: Math.min(rect.bottom, window.bottom) }
    }
  }
  return window
}
