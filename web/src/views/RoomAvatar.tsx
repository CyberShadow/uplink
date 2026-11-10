import { useState } from 'preact/hooks'
import type { Room } from '../generated/protocol'
import styles from './RoomAvatar.module.css'

/** Where the server serves the room's avatar `mxc://server/media`. */
export function avatarPath(room: Room): string | null {
  const match = room.avatar?.match(/^mxc:\/\/([^/]+)\/([^/]+)$/)
  if (!match) return null
  const [, server, media] = match
  return `/api/rooms/${room.id}/avatar/${encodeURIComponent(server)}/${encodeURIComponent(media)}`
}

/** What a room without an avatar shows, as Element does: its name's first
 * letter, on a colour of its id. */
export function initial(name: string): string {
  const letter = name.replace(/^[#@!+]/, '').match(/\p{L}|\p{N}/u)
  return (letter?.[0] ?? '?').toUpperCase()
}

const COLOURS = 6

export function colour(roomId: string): number {
  let sum = 0
  for (const char of roomId) sum += char.codePointAt(0)!
  return sum % COLOURS
}

/** The room's avatar: round, or of a space, a rounded square. */
export function RoomAvatar(props: { room: Room; class?: string }) {
  const { room } = props
  const path = avatarPath(room)
  const [failed, setFailed] = useState<string | null>(null)
  const shape = [styles.avatar, room.space && styles.space, props.class].filter(Boolean).join(' ')
  if (path && failed !== path) {
    return <img class={shape} src={path} alt="" loading="lazy" onError={() => setFailed(path)} />
  }
  return (
    <span class={`${shape} ${styles[`c${colour(room.room_id)}`]}`} aria-hidden="true">
      {initial(room.name)}
    </span>
  )
}
