// What had the focus in each place of the session's history: going back
// or forward to one gives it back (a room's box, a line, or nothing), as
// browsers give back where a page was scrolled. Each history entry is
// stamped with a key (preact-iso pushes them without state); what has the
// focus is kept by key, by the focused element's id.

/** By entry, the id of what had the focus last, or `null` for nothing
 * (or what has no id). */
const focused = new Map<string, string | null>()

function entry(): string | undefined {
  return (history.state as { focusKey?: string } | null)?.focusKey
}

const record = (element: EventTarget | null) => {
  const key = entry()
  if (key !== undefined) focused.set(key, element instanceof HTMLElement && element.id !== '' ? element.id : null)
}
document.addEventListener('focusin', (event) => record(event.target))
// Where the focus goes; to nothing, if `null`. (Where it goes elsewhere,
// a focusin follows.)
document.addEventListener('focusout', (event) => record(event.relatedTarget))

/** What to give the focus back to, once the entry gone back or forward to
 * is shown: as recorded before it renders (what leaves as it does is not
 * the entry's). `undefined` while there is nothing to give back. */
let restoring: string | null | undefined
window.addEventListener('popstate', () => {
  const key = entry()
  restoring = key === undefined ? undefined : focused.get(key)
})

/** Entries stamped in this page's life: with its start, a key of its own,
 * among those of pages before it, in the same history. */
let stamped = 0

/** Once a place is shown: stamps its entry, and gives the focus back, if
 * gone back or forward to; whether it did. */
export function restoreFocus(): boolean {
  if (entry() === undefined) history.replaceState({ ...history.state, focusKey: `${performance.timeOrigin}:${stamped++}` }, '')
  const id = restoring
  restoring = undefined
  if (id === undefined) return false
  if (id === null) (document.activeElement as HTMLElement).blur()
  // Gone (a line not loaded yet), it is left to the view.
  else document.getElementById(id)?.focus()
  return true
}
