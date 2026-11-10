// A Matrix room (SPEC §9): its timeline, the messages being sent to it,
// and a box to write one. Text messages show as text; other events, and
// message types uplink does not display, as their raw JSON.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { MatrixOutboxItem, MembershipChange, Room, RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Room.module.css'
import { dateTime } from '../format'

export function roomPath(room: number): string {
  return `/rooms/${room}`
}

export function RoomView(props: { room?: string }) {
  const { matrix } = useServices()
  const rooms = matrix.rooms.value
  const room = rooms?.find((r) => String(r.id) === props.room)
  if (!rooms) return <p class={styles.placeholder}>Loading…</p>
  if (!room) return <p class={styles.placeholder}>No such room.</p>
  return <Timeline key={room.id} room={room} />
}

/** Message types shown as text. */
const TEXT = new Set(['m.text', 'm.notice', 'm.emote'])

function Timeline(props: { room: Room }) {
  const { room } = props
  const { matrix } = useServices()
  const timeline = matrix.timeline(room.id)
  const events = timeline.events.value
  const pending = matrix.outbox.value?.filter((item) => item.room === room.id) ?? []
  const composer = useRef<HTMLTextAreaElement>(null)
  useKeymap({ reply: () => composer.current?.focus() })
  useMarkRead(room, events)

  return (
    <section class={styles.room} aria-label={room.name}>
      <header class={styles.header}>
        <h1>{room.name}</h1>
        {room.encrypted && <span class={styles.muted}>encrypted</span>}
        <MembershipControls room={room} />
      </header>
      {room.membership_error !== null && (
        <p class={styles.error} role="alert">
          The homeserver refused: {room.membership_error}
        </p>
      )}
      {/* Reversed, so that it stays scrolled to the latest event. */}
      <div class={styles.scroller}>
        <ol class={styles.events} aria-label="Timeline">
          {timeline.moreBefore.value && (
            <li>
              <button type="button" onMouseDown={keepFocus} onClick={() => timeline.loadEarlier()}>
                Load earlier events
              </button>
            </li>
          )}
          {events === null && <li class={styles.muted}>Loading…</li>}
          {events?.map((event) => <Event key={event.id} event={event} />)}
          {pending.map((item) => (
            <Pending key={item.id} item={item} />
          ))}
        </ol>
      </div>
      {room.membership === 'joined' && <Composer room={room} textarea={composer} />}
    </section>
  )
}

/** Where the account stands in the room, and the buttons to change it. */
function MembershipControls(props: { room: Room }) {
  const { room } = props
  const { matrix } = useServices()
  const change = (change: MembershipChange) => () => matrix.changeMembership(room.id, change)
  if (room.pending !== null) {
    return <span class={styles.muted}>{room.pending === 'join' ? 'Joining…' : 'Leaving…'}</span>
  }
  switch (room.membership) {
    case 'invited':
      return (
        <>
          <span class={styles.muted}>invited</span>
          <button type="button" onMouseDown={keepFocus} onClick={change('join')}>
            Join
          </button>
          <button type="button" onMouseDown={keepFocus} onClick={change('leave')}>
            Decline
          </button>
        </>
      )
    case 'joined':
      return (
        <button type="button" class={styles.leave} onMouseDown={keepFocus} onClick={change('leave')}>
          Leave
        </button>
      )
    case 'left':
      return <span class={styles.muted}>left</span>
  }
}

/** Marks the room read up to its latest event, while the tab is visible. */
function useMarkRead(room: Room, events: readonly RoomEvent[] | null) {
  const { matrix } = useServices()
  const [visible, setVisible] = useState(document.visibilityState === 'visible')
  useEffect(() => {
    const update = () => setVisible(document.visibilityState === 'visible')
    document.addEventListener('visibilitychange', update)
    return () => document.removeEventListener('visibilitychange', update)
  }, [])
  const last = events?.at(-1)?.id
  // What was marked, so that a room list reloaded meanwhile does not
  // mark it again.
  const marked = useRef<number | null>(null)
  useEffect(() => {
    if (!visible || last === undefined || last <= (room.read_up_to ?? 0) || marked.current === last) return
    marked.current = last
    matrix.markRead(room.id, last)
  }, [visible, last, room.read_up_to, room.id, matrix])
}

function Event(props: { event: RoomEvent }) {
  const { event } = props
  const sender = (
    <span class={styles.sender} title={event.sender}>
      {event.sender_name ?? event.sender}
    </span>
  )
  const time = (
    <time class={styles.time} dateTime={new Date(event.ts).toISOString()}>
      {dateTime(event.ts)}
    </time>
  )
  if (event.body !== null && event.msgtype !== null && TEXT.has(event.msgtype)) {
    return (
      <li class={styles.event}>
        {sender} {time}
        <div class={event.msgtype === 'm.notice' ? `${styles.body} ${styles.muted}` : styles.body}>
          {event.msgtype === 'm.emote' ? `* ${event.sender_name ?? event.sender} ${event.body}` : event.body}
        </div>
      </li>
    )
  }
  if (event.encryption === 'undecryptable') {
    return (
      <li class={styles.event}>
        {sender} {time}
        <div class={`${styles.body} ${styles.muted}`}>Unable to decrypt this message.</div>
      </li>
    )
  }
  return (
    <li class={styles.event}>
      <details>
        <summary>
          {sender} {time} <code>{event.msgtype ? `${event.kind} (${event.msgtype})` : event.kind}</code>
        </summary>
        <pre class={styles.raw}>{pretty(event.raw)}</pre>
      </details>
    </li>
  )
}

function pretty(raw: string): string {
  return JSON.stringify(JSON.parse(raw), null, 2)
}

function Pending(props: { item: MatrixOutboxItem }) {
  const { item } = props
  const { matrix } = useServices()
  const state = {
    queued: 'Waiting to send',
    sending: 'Sending…',
    sent: 'Sent',
    failed: `Not sent: ${item.error}`,
  }[item.state]
  return (
    <li class={`${styles.event} ${styles.pending}`}>
      <span class={styles.muted}>{state}</span>
      {(item.state === 'queued' || item.state === 'failed') && (
        <button type="button" class={styles.cancel} onMouseDown={keepFocus} onClick={() => matrix.cancelSend(item.id)}>
          Cancel
        </button>
      )}
      <div class={styles.body}>{item.body}</div>
    </li>
  )
}

function Composer(props: { room: Room; textarea: preact.RefObject<HTMLTextAreaElement> }) {
  const { matrix } = useServices()
  const [text, setText] = useState('')
  const send = async () => {
    if (text.trim() === '') return
    if (await matrix.send(props.room.id, text)) setText('')
  }
  return (
    <form
      class={styles.composer}
      onSubmit={(event) => {
        event.preventDefault()
        send()
      }}
    >
      <textarea
        ref={props.textarea}
        aria-label={`Message ${props.room.name}`}
        placeholder={`Message ${props.room.name}`}
        rows={2}
        value={text}
        onInput={(event) => setText(event.currentTarget.value)}
        onKeyDown={(event) => {
          // Enter sends; Shift+Enter is a new line.
          if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
            event.preventDefault()
            send()
          }
          if (event.key === 'Escape') event.currentTarget.blur()
        }}
      />
      <button type="submit" disabled={text.trim() === ''}>
        Send
      </button>
    </form>
  )
}
