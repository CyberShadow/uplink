// The regions of the window (SPEC §12.5): the parts that have the focus
// as a whole, as Slack's sections: the search box, the nav, a view, and
// in a room its timeline, its box and the panel beside it. F6 moves
// through them all, and each says where the focus goes in it.

import { type RefObject } from 'preact'
import { useLayoutEffect, useRef } from 'preact/hooks'
import { type Actions, lines } from './keymap'

/** `main`: the view, which is where the focus is while nothing has it,
 * and is passed over for the regions within it, where it has any. */
export type RegionName = 'search' | 'nav' | 'main' | 'timeline' | 'box' | 'panel'

/** Gives a region the focus: `last` is what had it there last, if still
 * there, for those that go back to it. */
type Enter = (last: HTMLElement | null) => void

interface Region {
  name: RegionName
  element: HTMLElement
  enter: Enter
  last: HTMLElement | null
}

const regions: Region[] = []

/** Makes `element` the region `name`, while the component is mounted. */
export function useRegion(name: RegionName, element: RefObject<HTMLElement>, enter: Enter) {
  // The handler changes with every render; the region need not.
  const current = useRef(enter)
  current.current = enter
  useLayoutEffect(() => {
    const region: Region = { name, element: element.current!, enter: (last) => current.current(last), last: null }
    const remember = (event: FocusEvent) => {
      region.last = event.target as HTMLElement
    }
    region.element.addEventListener('focusin', remember)
    regions.push(region)
    return () => {
      region.element.removeEventListener('focusin', remember)
      regions.splice(regions.indexOf(region), 1)
    }
  }, [name, element])
}

/** Whether the region has the focus, once given it: one hidden, or with
 * nothing to focus, has not. The view has it by nothing having it. */
function enter(region: Region): boolean {
  const { element, last } = region
  region.enter(last && element.contains(last) ? last : null)
  return region.name === 'main' ? document.activeElement === document.body : element.contains(document.activeElement)
}

/** Gives the region `name` the focus, if it is on show and takes it. */
export function enterRegion(name: RegionName): boolean {
  const region = regions.find((r) => r.name === name)
  return region !== undefined && enter(region)
}

/** Moves the focus to the region `by` after the one that has it, in the
 * order they show, around; from none, which is the view's, to the first
 * within the view, or to the one before it. */
export function cycleRegion(by: 1 | -1) {
  const main = regions.find((r) => r.name === 'main')!
  const within = (r: Region) => main.element.contains(r.element)
  const whole = !regions.some((r) => r !== main && within(r))
  const stops = regions
    .filter((r) => r !== main || whole)
    .sort((a, b) => (a.element.compareDocumentPosition(b.element) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1))
  let at = stops.findIndex((r) => r.element.contains(document.activeElement))
  if (at === -1) {
    const first = stops.findIndex(within)
    at = by === 1 && !whole ? first - 1 : first
  }
  for (let i = 1; i <= stops.length; i++) {
    if (enter(stops[(((at + by * i) % stops.length) + stops.length) % stops.length])) return
  }
}

/** What `selector` finds in `element`: what its keys move among. */
type Stops = () => HTMLElement[]

/** A region's keys, among its stops: j/k and the arrows to the next and
 * previous (from none, the first), Home and End to the first and last. */
export function stopKeys(stops: Stops): Actions {
  const move = (by: 1 | -1) => () => {
    const all = stops()
    const at = all.findIndex((s) => s.contains(document.activeElement))
    all[at === -1 ? 0 : Math.max(0, Math.min(at + by, all.length - 1))]?.focus()
  }
  return {
    ...lines(move(1), move(-1)),
    first: () => stops().at(0)?.focus(),
    last: () => stops().at(-1)?.focus(),
  }
}
