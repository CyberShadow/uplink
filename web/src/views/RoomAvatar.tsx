import { useState } from 'preact/hooks'
import type { Room } from '../generated/protocol'
import { mxcPath } from '../matrix'
import styles from './RoomAvatar.module.css'

/** Where the server serves the room's image `mxc://server/media`: its
 * avatar or its bridge's icon. */
function imagePath(room: Room, mxc: string): string | null {
  const path = mxcPath(mxc)
  return path && `/api/rooms/${room.id}/avatar/${path}`
}

/** Where the server serves the room's avatar. */
export function avatarPath(room: Room): string | null {
  return room.avatar === null ? null : imagePath(room, room.avatar)
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

/** The room's avatar: round, or of a space, a rounded square; of a
 * bridged room, with its network's icon as a badge at its corner, as
 * SchildiChat's room list has it, unless that icon is its avatar (as of
 * the space a bridge gathers its rooms in): the same image twice. */
export function RoomAvatar(props: { room: Room; class?: string }) {
  const { room } = props
  const bridge = room.bridge?.icon === room.avatar ? null : room.bridge
  const badge = bridge && imagePath(room, bridge.icon)
  const [failedBadge, setFailedBadge] = useState<string | null>(null)
  if (!bridge || !badge || failedBadge === badge) return <Avatar room={room} class={props.class} />
  return (
    <span class={[styles.badged, props.class].filter(Boolean).join(' ')}>
      <Avatar room={room} />
      <img
        class={styles.badge}
        src={badge}
        alt=""
        title={bridge.name}
        loading="lazy"
        onError={() => setFailedBadge(badge)}
      />
    </span>
  )
}

function Avatar(props: { room: Room; class?: string }) {
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
