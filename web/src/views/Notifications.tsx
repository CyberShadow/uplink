// Browser notifications of Matrix messages (SPEC §12.11), as Element's
// desktop notifications: when a room's unread messages that notify (by
// the account's push rules) grow while the room is not on show, its
// latest message, which a click opens.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { Room, RoomId } from '../generated/protocol'
import { useServices } from '../context'
import { displayName, shown } from '../matrix'
import type { MatrixStore } from '../stores/matrix'
import { Icon } from './Icon'
import { roomPath } from './Room'
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
