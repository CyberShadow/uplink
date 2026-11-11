// A Matrix room (SPEC §9): its timeline, the messages being sent to it,
// and a box to write one. Text messages show as text; other events, and
// message types uplink does not display, as their raw JSON.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { MatrixSend, MembershipChange, Room, RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Room.module.css'
import { RoomAvatar } from './RoomAvatar'
import { dateTime } from '../format'
import { sendSummary, text as message } from '../matrix'

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
  const sends = timeline.sends.value ?? []
  const composer = useRef<HTMLTextAreaElement>(null)
  useKeymap({ reply: () => composer.current?.focus() })
  useMarkRead(room, events)

  return (
    <section class={styles.room} aria-label={room.name}>
      <header class={styles.header}>
        <RoomAvatar room={room} class={styles.avatar} />
        <h1>{room.name}</h1>
        {room.encrypted && <span class={styles.muted}>encrypted</span>}
        <MembershipControls room={room} />
      </header>
      {room.membership_error !== null && (
        <p class={styles.error} role="alert">
          The homeserver refused: {room.membership_error}. <a href="/outbox">Undo the change</a> to keep the room
          as it is.
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
          {sends.map((item) => (
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
    const joining = room.pending === 'join'
    if (room.membership_error !== null) {
      return <span class={styles.muted}>{joining ? 'not joined' : 'not left'}</span>
    }
    return <span class={styles.muted}>{joining ? 'Joining…' : 'Leaving…'}</span>
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

function Pending(props: { item: MatrixSend }) {
  const { item } = props
  return (
    <li class={`${styles.event} ${styles.pending}`}>
      <span class={styles.muted}>{matrixSendState(item)}</span>
      <MatrixSendActions item={item} class={styles.cancel} />
      <div class={styles.body}>{sendSummary(item.content)}</div>
    </li>
  )
}

/** Where a message being sent is, in words. */
export function matrixSendState(item: MatrixSend): string {
  switch (item.state) {
    case 'queued':
      return 'Waiting to send'
    case 'sending':
      return item.wanted ? 'Sending…' : 'Sending; cancelling if it does not go out'
    case 'sent':
      return item.wanted ? 'Sent' : 'Sent before the cancel could take it back'
    case 'refused':
      return `Not sent: ${item.error}`
  }
}

/** Cancelling a message being sent, or keeping one sent though cancelled. */
export function MatrixSendActions(props: { item: MatrixSend; class?: string }) {
  const { item } = props
  const { matrix } = useServices()
  if (item.wanted && item.state !== 'sent') {
    return (
      <button type="button" class={props.class} onMouseDown={keepFocus} onClick={() => matrix.cancelSend(item.id)}>
        Cancel
      </button>
    )
  }
  if (!item.wanted && item.state === 'sent') {
    return (
      <button type="button" class={props.class} onMouseDown={keepFocus} onClick={() => matrix.keepSent(item.id)}>
        Keep it sent
      </button>
    )
  }
  return null
}

function Composer(props: { room: Room; textarea: preact.RefObject<HTMLTextAreaElement> }) {
  const { matrix } = useServices()
  const [text, setText] = useState('')
  const send = async () => {
    if (text.trim() === '') return
    if (await matrix.send(props.room.id, message(text))) setText('')
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
