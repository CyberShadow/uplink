// Where a list is scrolled, kept by its store as rows: the row at the
// top, and how far below the top it is. Shown again (from a conversation,
// or as a list kept), it is scrolled back there, though rows came or went
// above it meanwhile.

import type { RefObject } from 'preact'
import { useEffect, useLayoutEffect, useRef } from 'preact/hooks'
import type { Scrolled } from '../stores/mail'

/** Where it scrolls: in the app's content. */
function scrollerOf(list: HTMLElement): HTMLElement {
  return list.closest('main')!
}

/** The row at the top of the scroller, the first of `keys` (`list`'s
 * first children, in order) not above it; `null` if all are. */
function placeOf(list: HTMLElement, keys: readonly string[]): Scrolled | null {
  const top = scrollerOf(list).getBoundingClientRect().top
  const bottomOf = (index: number) => list.children[index].getBoundingClientRect().bottom
  let [low, high] = [0, keys.length]
  while (low < high) {
    const middle = (low + high) >> 1
    if (bottomOf(middle) <= top) low = middle + 1
    else high = middle
  }
  if (low === keys.length) return null
  return { key: keys[low], offset: list.children[low].getBoundingClientRect().top - top }
}

/** Scrolls `list` back to where it was. */
function scrollBack(list: HTMLElement, keys: readonly string[], { key, offset }: Scrolled) {
  const index = keys.indexOf(key)
  // Its row gone, it is where it is.
  if (index === -1) return
  const scroller = scrollerOf(list)
  scroller.scrollTop += list.children[index].getBoundingClientRect().top - scroller.getBoundingClientRect().top - offset
}

/** Keeps `place` told where `list`, of rows of `keys`, is scrolled, and
 * scrolls it back there as `shown` changes (another view's list shows),
 * or its rows show again: `list` is there while there are rows. */
export function useListPlace(
  list: RefObject<HTMLElement>,
  keys: readonly string[],
  shown: unknown,
  place: { scrolled: Scrolled | null },
) {
  const latest = useRef({ keys, place })
  latest.current = { keys, place }
  const present = keys.length > 0

  useLayoutEffect(() => {
    const { keys, place } = latest.current
    if (present && place.scrolled) scrollBack(list.current!, keys, place.scrolled)
  }, [list, shown, present])

  useEffect(() => {
    if (!present) return
    const element = list.current!
    const scroller = scrollerOf(element)
    // Once a frame, at most.
    let frame = 0
    const record = () => {
      frame = 0
      const { keys, place } = latest.current
      place.scrolled = placeOf(element, keys)
    }
    const scrolled = () => {
      if (frame === 0) frame = requestAnimationFrame(record)
    }
    scroller.addEventListener('scroll', scrolled, { passive: true })
    return () => {
      scroller.removeEventListener('scroll', scrolled)
      cancelAnimationFrame(frame)
    }
  }, [list, present])
}
