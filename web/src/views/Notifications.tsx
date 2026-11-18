// Browser notifications (SPEC §12.16): of what is unseen in the home
// buckets that notify, as urgently as their badges say, one per
// conversation or room, which a click opens, taken back once it is not;
// and reminders of events (SPEC §10.2), as Outlook's, which a click opens.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { CalendarEvent, Unseen, Urgency } from '../generated/protocol'
import { useServices } from '../context'
import { displayName, shown } from '../matrix'
import { before } from '../items'
import { Icon } from './Icon'
import { roomPath } from './Room'
import { calendarPath, defaultView } from './Calendar'
import { allMailPath } from './App'
import { date, time } from '../format'
import styles from './Nav.module.css'

/** Uplink's icon, on each notification: the title and body are only the
 * sender's and subject or the room's, so it is what tells them as uplink's
 * (in a desktop's notifications, with nothing else to tell their page). */
const ICON = '/logo-192.png'

/** How a notification calls for the user: interrupting, as what needs
 * the user, it stays until dismissed, with the system's sound; quiet, as
 * what is to be kept up with, it goes by, silently. One of what came
 * before, as an edit, is silent either way. */
function options(urgency: Urgency, quietly: boolean): NotificationOptions {
  switch (urgency) {
    case 'interrupting':
      return { requireInteraction: true, silent: quietly }
    case 'quiet':
      return { requireInteraction: false, silent: true }
  }
}

/** Notifies of what is unseen in the buckets that notify (SPEC §12.16),
 * one notification per conversation or room (`tag`), of its newest
 * message standing; and takes it back once none is. What was unseen as
 * the page opened is not notified of: the badges tell of it. */
export function useNotifications(route: (path: string) => void) {
  const { home } = useServices()
  // Of each urgency: the messages that came since the page opened, which
  // may be told of; and of each tag, its notification, and the message
  // it tells of.
  const of = useRef(new Map<Urgency, { told: Set<string>; standing: Map<string, Standing> }>())
  useEffect(
    () =>
      home.onUnseen((urgency, was, now) => {
        if (was === null) return
        if (!of.current.has(urgency)) of.current.set(urgency, { told: new Set(), standing: new Map() })
        const { told, standing } = of.current.get(urgency)!
        const tags = new Set<string>()
        for (const [key, listed] of now) {
          if (was.has(key)) continue
          told.add(key)
          tags.add(tagOf(listed.unseen))
        }
        for (const [key, listed] of was) {
          if (JSON.stringify(now.get(key)) === JSON.stringify(listed)) continue
          if (!now.has(key)) told.delete(key)
          tags.add(tagOf(listed.unseen))
        }
        if (!supported() || Notification.permission !== 'granted') return
        for (const tag of tags) {
          const newest = [...now.values()]
            .filter((listed) => told.has(listed.key) && tagOf(listed.unseen) === tag)
            .sort((a, b) => before(a.position, b.position))[0]
          const stands = standing.get(tag)
          if (newest === undefined) {
            stands?.notification.close()
            standing.delete(tag)
            continue
          }
          // As it was, of what it shows.
          if (stands?.key === newest.key && JSON.stringify(was.get(newest.key)) === JSON.stringify(newest)) continue
          // Of what came before, edited or standing again: quietly.
          const notification = notify(urgency, was.has(newest.key), newest.unseen, tag, route)
          standing.set(tag, { key: newest.key, notification })
        }
      }),
    [home, route],
  )
}

/** A notification shown, and the key of the message it tells of. */
interface Standing {
  key: string
  notification: Notification
}

/** One notification per conversation or room. */
function tagOf(unseen: Unseen): string {
  switch (unseen.kind) {
    case 'mail':
      return `mail-${unseen.conversation}`
    case 'message':
    case 'invite':
      return `room-${unseen.room}`
  }
}

/** Its title and body, and where a click goes. */
function shownOf(unseen: Unseen): { title: string; body: string; path: string } {
  switch (unseen.kind) {
    case 'mail': {
      const { entry } = unseen
      const sender = entry.from_name ?? entry.from_addr ?? '(unknown sender)'
      return {
        title: `${sender}: ${entry.subject ?? '(no subject)'}`,
        body: entry.snippet,
        path: allMailPath(entry.account, entry.id),
      }
    }
    case 'message': {
      const { event } = unseen
      return {
        title: unseen.name,
        body: `${displayName(event.sender, event.sender_name)}: ${shown(event).body ?? event.kind}`,
        path: roomPath(unseen.room),
      }
    }
    case 'invite': {
      const { invitation } = unseen
      return {
        title: unseen.name,
        body: `${displayName(invitation.inviter, invitation.inviter_name)} invites you`,
        path: roomPath(unseen.room),
      }
    }
  }
}

/** One per `tag`: a later one replaces it. */
function notify(
  urgency: Urgency,
  quietly: boolean,
  unseen: Unseen,
  tag: string,
  route: (path: string) => void,
): Notification {
  const { title, body, path } = shownOf(unseen)
  const notification = new Notification(title, { body, tag, icon: ICON, ...options(urgency, quietly) })
  notification.onclick = () => {
    window.focus()
    route(path)
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
    ...options('interrupting', false),
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
