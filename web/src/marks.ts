// The marks of the nav's places (SPEC §12.14), as the server pushes them,
// and their being seen (§12.15).

import { useEffect, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { useServices } from './context'
import type { Account, Badges, Mark, Place } from './generated/protocol'
import { allMailPath, inboxPath, mailPath } from './views/App'

export const NONE: Mark = { alert: 0, count: 0, dot: false }

/** The strongest of `marks`, as a fold shows what it folds: their counts
 * summed, else a dot if any has one. */
export function strongest(marks: Iterable<Mark>): Mark {
  let alert = 0
  let count = 0
  let dot = false
  for (const mark of marks) {
    alert += mark.alert
    count += mark.count
    dot ||= mark.dot
  }
  return { alert, count, dot }
}

/** The mark of `place`: All inboxes has its Inboxes'. */
export function markOf(badges: Badges | null, place: Place, accounts: readonly Account[]): Mark {
  if (badges === null) return NONE
  const { marks } = badges
  const of = <K>(list: [K, Mark][], key: K) => list.find(([k]) => k === key)?.[1] ?? NONE
  switch (place.kind) {
    case 'home':
      return marks.home
    case 'todos':
      return marks.todos
    case 'calendar':
      return marks.calendar
    case 'label':
      return of(marks.labels, place.label)
    case 'account':
      return of(marks.accounts, place.account)
    case 'room':
      return of(marks.rooms, place.room)
    case 'inboxes':
      return strongest(
        accounts.flatMap((a) => a.labels.filter(isInbox)).map((label) => of(marks.labels, label.id)),
      )
  }
}

export const isInbox = (label: { system: boolean; external_id: string }) =>
  label.system && label.external_id === 'INBOX'

/** Whether `href`'s view, or something in it, is on show at `path`. */
export function shows(path: string, href: string): boolean {
  return path === href || path.startsWith(`${href}/`)
}

/** The place on show at `path`, if it is one: a label's list, or a
 * conversation opened in it, is the label. */
export function placeOf(path: string, accounts: readonly Account[]): Place | null {
  if (path === '/') return { kind: 'home' }
  if (shows(path, inboxPath())) return { kind: 'inboxes' }
  if (shows(path, '/todos')) return { kind: 'todos' }
  if (shows(path, '/calendar')) return { kind: 'calendar' }
  const room = /^\/rooms\/(\d+)(\/|$)/.exec(path)
  if (room) return { kind: 'room', room: Number(room[1]) }
  for (const account of accounts) {
    if (shows(path, allMailPath(account.id))) return { kind: 'account', account: account.id }
    const label = account.labels.find((l) => shows(path, mailPath(account.id, l.external_id)))
    if (label) return { kind: 'label', label: label.id }
  }
  return null
}

/** Whether the tab is visible. */
export function useVisible(): boolean {
  const [visible, setVisible] = useState(document.visibilityState === 'visible')
  useEffect(() => {
    const update = () => setVisible(document.visibilityState === 'visible')
    document.addEventListener('visibilitychange', update)
    return () => document.removeEventListener('visibilitychange', update)
  }, [])
  return visible
}

/** Attends the place on show (SPEC §12.15), which sees what is in it:
 * while the tab is visible, its window focused, and the user has used it
 * (a key, the pointer, a touch) within the idle time; none otherwise.
 * Shows nothing: a component of its own, so that its changes render
 * nothing else. */
export function Attention() {
  const { home, mail, attend } = useServices()
  const { path } = useLocation()
  const place = placeOf(path, mail.accounts.value ?? [])
  const active = useActive(home.notifications.value?.idle_minutes ?? null)
  const attended = JSON.stringify(active ? place : null)
  useEffect(() => attend(JSON.parse(attended) as Place | null), [attended, attend])
  return null
}

/** What the user does by using a page. */
const USES = ['keydown', 'pointerdown', 'pointermove', 'wheel', 'touchstart'] as const

/** Whether the user is using the tab: it is visible, its window focused,
 * and they used it within `idleMinutes`; not before that is known. */
function useActive(idleMinutes: number | null): boolean {
  const [active, setActive] = useState(false)
  useEffect(() => {
    if (idleMinutes === null) return
    let idle: ReturnType<typeof setTimeout> | undefined
    // Coming back to it, as by focusing it, is using it.
    const used = () => {
      clearTimeout(idle)
      const looking = document.visibilityState === 'visible' && document.hasFocus()
      setActive(looking)
      if (looking) idle = setTimeout(() => setActive(false), idleMinutes * 60_000)
    }
    used()
    for (const use of USES) window.addEventListener(use, used, { capture: true, passive: true })
    window.addEventListener('focus', used)
    window.addEventListener('blur', used)
    document.addEventListener('visibilitychange', used)
    return () => {
      clearTimeout(idle)
      for (const use of USES) window.removeEventListener(use, used, { capture: true })
      window.removeEventListener('focus', used)
      window.removeEventListener('blur', used)
      document.removeEventListener('visibilitychange', used)
    }
  }, [idleMinutes])
  return active
}
