// Browser notifications (SPEC §12.16): of what comes from others into
// the badged home buckets, as urgently as their badges say, which a
// click opens; and reminders of events (SPEC §10.2), as Outlook's, which
// a click opens.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { Arrival, Badge, CalendarEvent, Entry, RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import { displayName, shown } from '../matrix'
import { Icon } from './Icon'
import { roomPath } from './Room'
import { calendarPath, defaultView } from './Calendar'
import { allMailPath } from './App'
import { messagePath } from './Todos'
import { date, time } from '../format'
import { shows } from './Nav'
import styles from './Nav.module.css'

/** How a notification calls for the user: of a `count` bucket's, as what
 * needs the user, it stays until dismissed, with the system's sound; of a
 * `dot` bucket's, as what is to be kept up with, it goes by, silently. */
function urgency(badge: Badge): NotificationOptions {
  switch (badge) {
    case 'count':
      return { requireInteraction: true, silent: false }
    case 'dot':
      return { requireInteraction: false, silent: true }
  }
}

/** Notifies of what comes into the badged buckets, but what is on show. */
export function useNotifications(path: string, route: (path: string) => void) {
  const { home, mail, matrix } = useServices()
  const where = useRef(path)
  where.current = path
  useEffect(
    () =>
      home.onArrival(async (arrival: Arrival) => {
        if (!supported() || Notification.permission !== 'granted') return
        const item = arrival.item
        switch (item.kind) {
          case 'mail': {
            const entries = await mail.conversation(item.entry)
            if (!document.hidden && entries.some((e) => showsEntry(where.current, e))) return
            const entry = entries.find((e) => e.id === item.entry)!
            const sender = entry.from_name ?? entry.from_addr ?? '(unknown sender)'
            const title = `${sender}: ${entry.subject ?? '(no subject)'}`
            const tag = `mail-${entry.account}-${entry.thread_id ?? entry.id}`
            notify(arrival.badge, title, entry.snippet, tag, () => route(allMailPath(entry.account, entry.id)))
            return
          }
          case 'message': {
            if (!document.hidden && shows(where.current, roomPath(item.room))) return
            const event = await matrix.event(item.room, item.event)
            // Redacted since.
            if (event === null) return
            const room = matrix.rooms.value!.find((r) => r.id === item.room)!
            notify(arrival.badge, room.name, body(event), `room-${item.room}`, () =>
              route(roomPath(item.room)),
            )
            return
          }
        }
      }),
    [home, mail, matrix, route],
  )
}

/** Of each list of mail, how many segments its path has with an entry
 * opened, the last (as `App`'s routes have them). */
const OPENED_IN: Record<string, number> = { inbox: 2, all: 3, search: 3, mail: 4 }

/** Whether `path` shows the entry's conversation, opened by it. */
function showsEntry(path: string, entry: Entry): boolean {
  const segments = path.split('/').slice(1)
  const opened = OPENED_IN[segments[0]] === segments.length && segments.at(-1) === String(entry.id)
  return opened || path === messagePath(entry.message)
}

function body(event: RoomEvent): string {
  return `${displayName(event.sender, event.sender_name)}: ${shown(event).body ?? event.kind}`
}

/** One per `tag`: a later one replaces it. */
function notify(badge: Badge, title: string, body: string, tag: string, open: () => void) {
  const notification = new Notification(title, { body, tag, ...urgency(badge) })
  notification.onclick = () => {
    window.focus()
    open()
    notification.close()
  }
}

/** Reminds of events as their reminders fall due, as Outlook does: each
 * once (a reminder already due, at once, unless the event has begun);
 * not of those cancelled (those declined are not listed). */
export function useReminders(route: (path: string) => void) {
  const { calendar } = useServices()
  useEffect(() => calendar.watchUpcoming(), [calendar])
  const events = calendar.upcoming.value
  // Of each event reminded of, its start: one moved is reminded of anew.
  const reminded = useRef(new Set<string>())
  useEffect(() => {
    if (!events || !supported() || Notification.permission !== 'granted') return
    const timers = events
      .filter((e) => e.reminder !== null && !e.cancelled)
      .filter((e) => !reminded.current.has(`${e.id}@${e.start}`))
      .map((e) => {
        const remind = () => {
          // Late, as after the computer slept: not once it has begun.
          if (Date.now() >= e.start) return
          reminded.current.add(`${e.id}@${e.start}`)
          notifyOf(e, route)
        }
        return setTimeout(remind, Math.max(0, e.start - e.reminder! * 60_000 - Date.now()))
      })
    return () => timers.forEach(clearTimeout)
  }, [events, route])
}

function notifyOf(event: CalendarEvent, route: (path: string) => void) {
  const minutes = Math.round((event.start - Date.now()) / 60_000)
  const when = event.all_day ? `${date(new Date(event.start))}, all day` : `${time(event.start)}–${time(event.end)}`
  const body = [when, minutes > 0 && `in ${minutes} min`, event.location].filter(Boolean).join(' · ')
  // As urgent as what needs the user: it stays until dismissed.
  const notification = new Notification(event.subject, { body, tag: `event-${event.id}`, ...urgency('count') })
  notification.onclick = () => {
    window.focus()
    route(calendarPath(defaultView(), date(new Date(event.start)), event.id))
    notification.close()
  }
}

function supported(): boolean {
  return typeof Notification !== 'undefined'
}

/** Asks the browser to let uplink notify, until it is answered. */
export function NotificationsItem() {
  const [permission, setPermission] = useState(supported() ? Notification.permission : 'denied')
  if (permission !== 'default') return null
  return (
    <li>
      <button
        type="button"
        class={styles.action}
        onClick={async () => setPermission(await Notification.requestPermission())}
      >
        <Icon name="bell" />
        <span class={styles.name}>Turn on notifications</span>
      </button>
    </li>
  )
}
