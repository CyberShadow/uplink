// Browser notifications (SPEC §12.16): of what comes from others into
// the home buckets that notify, as urgently as their badges say, which a
// click opens, taken back once no news; and reminders of events (SPEC
// §10.2), as Outlook's, which a click opens.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { Arrived, CalendarEvent, Entry, RoomEvent, Urgency } from '../generated/protocol'
import type { News } from '../stores/home'
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

/** Uplink's icon, on each notification: the title and body are only the
 * sender's and subject or the room's, so it is what tells them as uplink's
 * (in a desktop's notifications, with nothing else to tell their page). */
const ICON = '/logo-192.png'

/** How a notification calls for the user: interrupting, as what needs
 * the user, it stays until dismissed, with the system's sound; quiet, as
 * what is to be kept up with, it goes by, silently. */
function options(urgency: Urgency): NotificationOptions {
  switch (urgency) {
    case 'interrupting':
      return { requireInteraction: true, silent: false }
    case 'quiet':
      return { requireInteraction: false, silent: true }
  }
}

/** A notification to show: one per `tag`, a later one replacing it. */
interface Shown {
  title: string
  body: string
  tag: string
  open: () => void
}

/** Notifies of what comes into the badged buckets, but what is on show;
 * and takes a notification back once what it tells of is no news. */
export function useNotifications(path: string, route: (path: string) => void) {
  const { home, mail, matrix } = useServices()
  const where = useRef(path)
  where.current = path
  // Of each tag, its notification, and what it tells of.
  const standing = useRef(new Map<string, { told: string; notification: Notification }>())
  useEffect(() => {
    /** What to show of `item`, unless it is on show. */
    const shown = async (item: Arrived): Promise<Shown | null> => {
      switch (item.kind) {
        case 'mail': {
          const entries = await mail.conversation(item.entry)
          if (!document.hidden && entries.some((e) => showsEntry(where.current, e))) return null
          const entry = entries.find((e) => e.id === item.entry)!
          const sender = entry.from_name ?? entry.from_addr ?? '(unknown sender)'
          return {
            title: `${sender}: ${entry.subject ?? '(no subject)'}`,
            body: entry.snippet,
            tag: `mail-${entry.account}-${entry.thread_id ?? entry.id}`,
            open: () => route(allMailPath(entry.account, entry.id)),
          }
        }
        case 'message': {
          if (!document.hidden && shows(where.current, roomPath(item.room))) return null
          const event = await matrix.event(item.room, item.event)
          // Redacted since.
          if (event === null) return null
          const room = matrix.rooms.value!.find((r) => r.id === item.room)!
          return {
            title: room.name,
            body: body(event),
            tag: `room-${item.room}`,
            open: () => route(roomPath(item.room)),
          }
        }
        case 'invite':
          if (!document.hidden && shows(where.current, roomPath(item.room))) return null
          return {
            title: item.name,
            body: 'Invites you',
            tag: `room-${item.room}`,
            open: () => route(roomPath(item.room)),
          }
      }
    }
    return home.onNews(async (news: News) => {
      switch (news.type) {
        case 'arrival': {
          if (!supported() || Notification.permission !== 'granted') return
          const show = await shown(news.item)
          if (show === null) return
          const notification = notify(news.urgency, show.title, show.body, show.tag, show.open)
          standing.current.set(show.tag, { told: told(news.item), notification })
          return
        }
        case 'recall':
          for (const [tag, { told: of, notification }] of standing.current) {
            if (of !== told(news.item)) continue
            notification.close()
            standing.current.delete(tag)
          }
          return
      }
    })
  }, [home, mail, matrix, route])
}

/** What an arrival tells of, as a recall names it. */
function told(item: Arrived): string {
  switch (item.kind) {
    case 'mail':
      return `mail-${item.entry}`
    case 'message':
      return `message-${item.event}`
    case 'invite':
      return `invite-${item.room}`
  }
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
function notify(urgency: Urgency, title: string, body: string, tag: string, open: () => void): Notification {
  const notification = new Notification(title, { body, tag, icon: ICON, ...options(urgency) })
  notification.onclick = () => {
    window.focus()
    open()
    notification.close()
  }
  return notification
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
  const notification = new Notification(event.subject, {
    body,
    tag: `event-${event.id}`,
    icon: ICON,
    ...options('interrupting'),
  })
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
