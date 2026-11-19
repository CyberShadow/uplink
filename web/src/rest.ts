// Where the focus rests (SPEC §12.5): something always has it, so that
// Tab goes on from where one is, and Esc, pressed again and again, ends
// at a known element, not at none. A view's list has it, as Gmail's; a
// room's messages, with none chosen; else, the view itself.

import { type RefObject } from 'preact'
import { useLayoutEffect } from 'preact/hooks'

/** The elements the focus may rest on, of what is on show: the view, and
 * what within it takes its place (a list, a room's messages). */
const rests = new Set<HTMLElement>()

/** Where it rests: on the innermost. Before the app shows, nowhere. */
function place(): HTMLElement | undefined {
  return [...rests].find((r) => ![...rests].some((other) => other !== r && r.contains(other)))
}

/** Makes `element` where the focus rests, while the component is mounted:
 * in place of the one around it, from which it takes the focus if that
 * has it (a list, once loaded, from the view). */
export function useRest(element: RefObject<HTMLElement>) {
  useLayoutEffect(() => {
    const el = element.current!
    const before = place()
    rests.add(el)
    if (before !== undefined && document.activeElement === before) rest()
    return () => {
      const had = document.activeElement === el
      rests.delete(el)
      if (had) rest()
    }
  }, [element])
}

/** Whether the focus is at rest. */
export function resting(): boolean {
  return document.activeElement === place()
}

/** Puts the focus at rest: what Esc does to what has it. */
export function rest() {
  place()?.focus({ preventScroll: true })
}

// What has the focus may go from under it (a line deleted, a dialog
// closed whose opener is gone), and a click on what takes none gives it
// to the page: browsers tell of neither. The key pressed next is the
// resting element's, and Tab goes on from it.
window.addEventListener(
  'keydown',
  () => {
    if (document.activeElement === document.body) rest()
  },
  true,
)
