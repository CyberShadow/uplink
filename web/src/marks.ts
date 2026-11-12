// The marks of the nav's places (SPEC §12.14), as the server pushes them,
// and their being seen (§12.15).

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { useServices } from './context'
import type { Account, Badges, Mark, Place } from './generated/protocol'
import { allMailPath, inboxPath, mailPath } from './views/App'

export const NONE: Mark = { count: 0, dot: false }

/** The strongest of `marks`, as a fold shows what it folds: their counts
 * summed, else a dot if any has one. */
export function strongest(marks: Iterable<Mark>): Mark {
  let count = 0
  let dot = false
  for (const mark of marks) {
    count += mark.count
    dot ||= mark.dot
  }
  return { count, dot }
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

/** Sees the place on show, in a visible tab, while its mark has a dot:
 * once shown, and again as something new comes to it. Once for each
 * push of the badges, which the server pushes anew once it is seen. */
export function useSeen() {
  const { home, mail, ops } = useServices()
  const { path } = useLocation()
  const visible = useVisible()
  const accounts = mail.accounts.value ?? []
  const place = placeOf(path, accounts)
  const badges = home.badges.value
  const dot = place !== null && markOf(badges, place, accounts).dot
  const key = JSON.stringify(place)
  // The place last seen, and as of which push.
  const sent = useRef<{ key: string; badges: Badges | null } | null>(null)
  useEffect(() => {
    if (!visible || !dot || (sent.current?.key === key && sent.current.badges === badges)) return
    sent.current = { key, badges }
    void ops.dispatch({ type: 'mark_seen', place: JSON.parse(key) as Place }, true)
  }, [visible, dot, badges, key, ops])
}
