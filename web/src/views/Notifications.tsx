// Browser notifications: of Matrix messages (SPEC §12.11), as Element's
// desktop notifications: when a room's unread messages that notify (by
// the account's push rules) grow while the room is not on show, its
// latest message, which a click opens; and reminders of events (SPEC
// §10.2), as Outlook's, which a click opens.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { CalendarEvent, Room, RoomId } from '../generated/protocol'
import { useServices } from '../context'
import { displayName, shown } from '../matrix'
import type { MatrixStore } from '../stores/matrix'
import { Icon } from './Icon'
import { roomPath } from './Room'
import { calendarPath, defaultView } from './Calendar'
import { date, time } from '../format'
import { shows } from './Nav'
import styles from './Nav.module.css'

/** Notifies of the rooms' messages while the user is elsewhere. */
export function useNotifications(path: string, route: (path: string) => void) {
  const { matrix } = useServices()
  const rooms = matrix.rooms.value
  // Of each room, how many notified when last seen; `null` before the
  // rooms are first loaded, whose unread messages are not news.
  const seen = useRef<Map<RoomId, number> | null>(null)
  const where = useRef(path)
  where.current = path
  useEffect(() => {
    if (!rooms) return
    const previous = seen.current
    seen.current = new Map(rooms.map((room) => [room.id, room.notifying]))
    if (previous === null || !supported() || Notification.permission !== 'granted') return
    for (const room of rooms) {
      if (room.notifying <= (previous.get(room.id) ?? 0)) continue
      if (!document.hidden && shows(where.current, roomPath(room.id))) continue
      notify(matrix, room, route)
    }
  }, [rooms, matrix, route])
}

async function notify(matrix: MatrixStore, room: Room, route: (path: string) => void) {
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const latest = await matrix.latestMessage(room.id, me)
  const body =
    latest === null
      ? `${room.notifying} new messages`
      : `${displayName(latest.sender, latest.sender_name)}: ${shown(latest).body ?? latest.kind}`
  // One per room: a later one replaces it.
  const notification = new Notification(room.name, { body, tag: `room-${room.id}` })
  notification.onclick = () => {
    window.focus()
    route(roomPath(room.id))
    notification.close()
  }
}

/** Reminds of events as their reminders fall due, as Outlook does: each
 * once (a reminder already due, at once, unless the event has begun);
 * not of those cancelled or declined. */
export function useReminders(route: (path: string) => void) {
  const { calendar } = useServices()
  useEffect(() => calendar.watchUpcoming(), [calendar])
  const events = calendar.upcoming.value
  // Of each event reminded of, its start: one moved is reminded of anew.
  const reminded = useRef(new Set<string>())
  useEffect(() => {
    if (!events || !supported() || Notification.permission !== 'granted') return
    const timers = events
      .filter((e) => e.reminder !== null && !e.cancelled && e.response !== 'declined')
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
  const notification = new Notification(event.subject, { body, tag: `event-${event.id}` })
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
