// Starting a conversation (SPEC §9.7, §12.11), as Element's "+" menu
// does: creating a room, opening a direct chat, or joining a room by its
// address. Once the homeserver has, the room opens as soon as sync lists
// it.

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { useServices } from '../context'
import { roomPath } from './Room'
import overlay from './Overlay.module.css'
import styles from './RoomStarter.module.css'

type Mode = 'room' | 'direct' | 'join'

const MODES: [Mode, string][] = [
  ['room', 'New room'],
  ['direct', 'Direct chat'],
  ['join', 'Join a room'],
]

/** User ids, as typed: separated by commas or spaces. */
export function userIds(text: string): string[] {
  return text.split(/[\s,]+/).filter((id) => id !== '')
}

/** Opens a room the account joined, once sync lists it, and calls
 * `opened`: the room's Matrix id being opened (`null` while none), and
 * its setter. */
export function useOpenJoined(account: string, opened: () => void): [string | null, (roomId: string) => void] {
  const { matrix } = useServices()
  const { route } = useLocation()
  const [joined, setJoined] = useState<string | null>(null)
  const rooms = matrix.rooms.value
  useEffect(() => {
    const room = joined === null ? undefined : rooms?.find((r) => r.account === account && r.room_id === joined)
    if (!room) return
    setJoined(null)
    opened()
    route(roomPath(room.id))
  }, [joined, rooms, account, opened, route])
  return [joined, setJoined]
}

export function RoomStarter(props: { account: string; close: () => void }) {
  const { matrix } = useServices()
  const [mode, setMode] = useState<Mode>('room')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [joined, setJoined] = useOpenJoined(props.account, props.close)

  const start = async (request: Parameters<typeof matrix.join>[0]) => {
    setBusy(true)
    setError(null)
    try {
      setJoined(await matrix.join(request))
    } catch (e) {
      setError((e as Error).message)
      setBusy(false)
    }
  }

  return (
    <div class={overlay.backdrop} onClick={props.close}>
      <div
        class={`${overlay.dialog} ${styles.starter}`}
        role="dialog"
        aria-label="Start a conversation"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key !== 'Escape') return
          e.stopPropagation()
          props.close()
        }}
      >
        <div class={styles.modes} role="tablist">
          {MODES.map(([m, label]) => (
            <button
              key={m}
              type="button"
              role="tab"
              aria-selected={mode === m}
              onClick={() => {
                setMode(m)
                setError(null)
              }}
            >
              {label}
            </button>
          ))}
        </div>
        {mode === 'room' && (
          <NewRoomForm
            busy={busy}
            start={(room) =>
              start({
                type: 'create_matrix_room',
                account: props.account,
                room,
              })
            }
          />
        )}
        {mode === 'direct' && (
          <OneField
            label="User"
            placeholder="@user:example.org"
            action="Start chat"
            busy={busy}
            submit={(user) => start({ type: 'start_direct_chat', account: props.account, user })}
          />
        )}
        {mode === 'join' && (
          <OneField
            label="Address or id"
            placeholder="#room:example.org"
            action="Join"
            busy={busy}
            submit={(room) => start({ type: 'join_matrix_room', account: props.account, room })}
          />
        )}
        {error !== null && (
          <p class={styles.error} role="alert">
            {error}
          </p>
        )}
        {joined !== null && <p class={styles.muted}>Waiting for the room to sync…</p>}
      </div>
    </div>
  )
}

function NewRoomForm(props: {
  busy: boolean
  start: (room: {
    name: string
    topic: string | null
    invite: string[]
    encrypted: boolean
    alias: string | null
  }) => void
}) {
  const [name, setName] = useState('')
  const [topic, setTopic] = useState('')
  const [invite, setInvite] = useState('')
  const [isPublic, setPublic] = useState(false)
  const [alias, setAlias] = useState('')
  const [encrypted, setEncrypted] = useState(true)
  const first = useAutofocus()
  return (
    <form
      class={styles.form}
      onSubmit={(event) => {
        event.preventDefault()
        props.start({
          name,
          topic: topic.trim() || null,
          invite: userIds(invite),
          encrypted,
          alias: isPublic ? alias.trim() : null,
        })
      }}
    >
      <label>
        Name
        <input ref={first} value={name} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <label>
        Topic
        <input value={topic} onInput={(e) => setTopic(e.currentTarget.value)} />
      </label>
      <label>
        Invite
        <input placeholder="@user:example.org, …" value={invite} onInput={(e) => setInvite(e.currentTarget.value)} />
      </label>
      <label class={styles.check}>
        <input
          type="checkbox"
          checked={isPublic}
          onChange={(e) => {
            setPublic(e.currentTarget.checked)
            // As Element: public rooms are not encrypted by default.
            setEncrypted(!e.currentTarget.checked)
          }}
        />
        Public: anyone may find and join it, at its address
      </label>
      {isPublic && (
        <label>
          Address
          <span class={styles.alias}>
            #
            <input aria-label="Address" value={alias} onInput={(e) => setAlias(e.currentTarget.value)} />
          </span>
        </label>
      )}
      <label class={styles.check}>
        <input type="checkbox" checked={encrypted} onChange={(e) => setEncrypted(e.currentTarget.checked)} />
        Encrypted (cannot be turned off later)
      </label>
      <div class={styles.actions}>
        <button type="submit" disabled={props.busy || (isPublic && alias.trim() === '')}>
          Create room
        </button>
      </div>
    </form>
  )
}

function OneField(props: {
  label: string
  placeholder: string
  action: string
  busy: boolean
  submit: (value: string) => void
}) {
  const [value, setValue] = useState('')
  const input = useAutofocus()
  return (
    <form
      class={styles.form}
      onSubmit={(event) => {
        event.preventDefault()
        props.submit(value.trim())
      }}
    >
      <label>
        {props.label}
        <input
          ref={input}
          placeholder={props.placeholder}
          value={value}
          onInput={(e) => setValue(e.currentTarget.value)}
        />
      </label>
      <div class={styles.actions}>
        <button type="submit" disabled={props.busy || value.trim() === ''}>
          {props.action}
        </button>
      </div>
    </form>
  )
}

function useAutofocus() {
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => input.current?.focus(), [])
  return input
}
