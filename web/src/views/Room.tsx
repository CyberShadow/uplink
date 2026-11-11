// A Matrix room (SPEC §9, §12.11), laid out as IRC clients do, and as
// Element's IRC layout: a line per event, its time, sender and body side
// by side; days apart; a line where the unread begin. Messages being
// sent follow, and the box to write one.

import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import type {
  GapId,
  MatrixSend,
  MemberReceipt,
  MembershipChange,
  ReadMarker,
  Room,
  RoomEvent,
  RoomEventId,
  RoomMember,
  TimelineGap,
} from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Room.module.css'
import { RoomAvatar } from './RoomAvatar'
import { date } from '../format'
import { TEXT, displayName, sendSummary, shown } from '../matrix'
import { EventRow, MembershipRun, type Overlay } from './RoomEvent'
import { Composer, type Mode } from './RoomComposer'
import { RoomInfo } from './RoomInfo'

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

function Timeline(props: { room: Room }) {
  const { room } = props
  const { matrix } = useServices()
  const timeline = matrix.timeline(room.id)
  const events = timeline.events.value
  const sends = timeline.sends.value
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const composer = useRef<HTMLTextAreaElement>(null)
  const [mode, setMode] = useState<Mode>({ type: 'new' })
  // Where the unread began when the room was opened: reading it moves
  // the marker, but not the line, while it is on show.
  const [readUpTo] = useState(room.read_up_to)
  const [files, setFiles] = useState<File[]>([])
  const [info, setInfo] = useState(false)
  useKeymap({ reply: () => composer.current?.focus() })
  useMarkRead(room, events)

  const [overlay, outgoing] = useMemo(() => overlaid(events ?? [], sends ?? []), [events, sends])
  const receipts = timeline.receipts.value
  const readers = useMemo(() => readersByEvent(events ?? [], receipts), [events, receipts])
  const gaps = timeline.gaps.value
  const list = useMemo(() => rows(events ?? [], readUpTo, me, gaps), [events, readUpTo, me, gaps])
  const lastOwn = useMemo(
    () =>
      events?.findLast(
        (e) => e.sender === me && e.kind === 'm.room.message' && !e.redacted && TEXT.has(shown(e).msgtype ?? ''),
      ) ?? null,
    [events, me],
  )
  const act = (next: Mode) => {
    setMode(next)
    composer.current?.focus()
  }

  return (
    <div class={styles.split}>
      <section
        class={styles.room}
        aria-label={room.name}
        onDragOver={(event) => {
          if (room.membership === 'joined' && event.dataTransfer?.types.includes('Files')) event.preventDefault()
        }}
        onDrop={(event) => {
          if (room.membership !== 'joined' || !event.dataTransfer?.files.length) return
          event.preventDefault()
          setFiles([...files, ...event.dataTransfer.files])
        }}
      >
        <header class={styles.header}>
          <RoomAvatar room={room} class={styles.avatar} />
          <div class={styles.title}>
            <h1>{room.name}</h1>
            {room.topic !== null && (
              <p class={styles.topic} title={room.topic}>
                {room.topic}
              </p>
            )}
          </div>
          <span class={styles.muted}>
            <button
              type="button"
              class={styles.link}
              aria-expanded={info}
              onMouseDown={keepFocus}
              onClick={() => setInfo(!info)}
            >
              {room.joined_members === 1 ? '1 member' : `${room.joined_members} members`}
            </button>
            {room.encrypted && ' · encrypted'}
          </span>
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
              <li class={styles.earlier}>
                <button type="button" onMouseDown={keepFocus} onClick={() => timeline.loadEarlier()}>
                  Load earlier events
                </button>
              </li>
            )}
            {events === null && <li class={styles.muted}>Loading…</li>}
            {list.map((row) => {
              switch (row.type) {
                case 'day':
                  return (
                    <li key={row.key} class={styles.day} role="separator">
                      <span>{row.date}</span>
                    </li>
                  )
                case 'unread':
                  return (
                    <li key={row.key} class={styles.unread} role="separator" aria-label="New messages">
                      <span>New</span>
                    </li>
                  )
                case 'members':
                  return <MembershipRun key={row.key} room={room} events={row.events} />
                case 'gap': {
                  const filling = timeline.filling.value.has(row.gap)
                  return (
                    <li key={row.key} class={styles.gap} role="separator" aria-label="Events missing">
                      <span>Earlier events are missing here.</span>
                      <button
                        type="button"
                        disabled={filling}
                        onMouseDown={keepFocus}
                        onClick={() => timeline.fill(row.gap)}
                      >
                        {filling ? 'Fetching…' : 'Fetch them'}
                      </button>
                    </li>
                  )
                }
                case 'event':
                  return (
                    <EventRow
                      key={row.key}
                      room={room}
                      event={row.event}
                      me={me}
                      overlay={overlay.get(row.event.event_id)}
                      readers={readers.get(row.event.id) ?? NO_READERS}
                      pendingRedactions={overlay.redacted}
                      reply={() => act({ type: 'reply', event: row.event })}
                      edit={() => act({ type: 'edit', event: row.event })}
                    />
                  )
              }
            })}
            {outgoing.map((item) => (
              <Pending key={item.id} item={item} />
            ))}
          </ol>
        </div>
        <Typing users={timeline.typing.value} />
        {room.membership === 'joined' && (
          <Composer
            room={room}
            textarea={composer}
            mode={mode}
            setMode={setMode}
            lastOwn={lastOwn}
            files={files}
            setFiles={setFiles}
          />
        )}
      </section>
      {info && <RoomInfo room={room} close={() => setInfo(false)} />}
    </div>
  )
}

/** Who is typing, in a line above the composer. */
function Typing(props: { users: readonly RoomMember[] }) {
  const names = props.users.map((u) => displayName(u.user, u.name))
  return (
    <p class={styles.typing} aria-live="polite">
      {names.length === 0
        ? ''
        : names.length === 1
          ? `${names[0]} is typing…`
          : names.length <= 3
            ? `${names.slice(0, -1).join(', ')} and ${names.at(-1)} are typing…`
            : `${names.slice(0, 2).join(', ')} and ${names.length - 2} others are typing…`}
    </p>
  )
}

const NO_READERS: readonly MemberReceipt[] = []

/** Members' receipts by the loaded event each shows on: the last listed
 * at or before the event it is for (which may not be listed itself, as a
 * reaction is not), as Element shows them. */
export function readersByEvent(
  events: readonly RoomEvent[],
  receipts: readonly MemberReceipt[],
): Map<RoomEventId, MemberReceipt[]> {
  const byEvent = new Map<RoomEventId, MemberReceipt[]>()
  for (const receipt of receipts) {
    const at = events.findLast((e) => e.pos <= receipt.pos)
    if (!at) continue
    const readers = byEvent.get(at.id) ?? []
    readers.push(receipt)
    byEvent.set(at.id, readers)
  }
  return byEvent
}

type Row =
  | { type: 'day'; key: string; date: string }
  | { type: 'unread'; key: string }
  | { type: 'event'; key: string; event: RoomEvent }
  | { type: 'members'; key: string; events: RoomEvent[] }
  | { type: 'gap'; key: string; gap: GapId }

/** Membership changes in a row that are folded into one, as Element
 * folds them: at least these many. */
const RUN = 3

/** The timeline's rows: a day's line before its first event, the unread
 * line before the first event from others after `readUpTo`, runs of
 * membership changes folded, and the gaps where events are missing. */
export function rows(
  events: readonly RoomEvent[],
  readUpTo: ReadMarker | null,
  me: string,
  gaps: readonly TimelineGap[] = [],
): Row[] {
  const out: Row[] = []
  let day: string | null = null
  let run: RoomEvent[] = []
  let unreadShown = readUpTo === null
  const gapBefore = new Map(gaps.flatMap((g) => (g.before === null ? [] : [[g.before, g.id] as const])))
  const flush = () => {
    if (run.length >= RUN) out.push({ type: 'members', key: `members-${run[0].id}`, events: run })
    else for (const event of run) out.push({ type: 'event', key: String(event.id), event })
    run = []
  }
  for (const event of events) {
    const eventDay = date(new Date(event.ts))
    const unread = !unreadShown && event.pos > readUpTo!.pos && event.sender !== me
    const gap = gapBefore.get(event.id)
    if (eventDay !== day || unread || gap !== undefined) flush()
    if (gap !== undefined) out.push({ type: 'gap', key: `gap-${gap}`, gap })
    if (eventDay !== day) {
      out.push({ type: 'day', key: `day-${event.id}`, date: eventDay })
      day = eventDay
    }
    if (unread) {
      out.push({ type: 'unread', key: 'unread' })
      unreadShown = true
    }
    if (event.kind === 'm.room.member' && !event.redacted) {
      run.push(event)
    } else {
      flush()
      out.push({ type: 'event', key: String(event.id), event })
    }
  }
  flush()
  for (const gap of gaps) if (gap.before === null) out.push({ type: 'gap', key: `gap-${gap.id}`, gap: gap.id })
  return out
}

/** Sends that change what loaded events show — reactions, edits and
 * deletions — are shown on those, as Element does, while they go out.
 * Those that need a decision (refused, or sent though cancelled) are
 * listed with the room's other sends, at the end, with their buttons. */
function overlaid(events: readonly RoomEvent[], sends: readonly MatrixSend[]): [Overlays, MatrixSend[]] {
  const loaded = new Set<string>()
  for (const event of events) {
    loaded.add(event.event_id)
    for (const reaction of event.reactions) for (const reactor of reaction.senders) loaded.add(reactor.event_id)
  }
  const overlays = new Overlays()
  const outgoing: MatrixSend[] = []
  for (const item of sends) {
    const content = item.content
    const target =
      content.type === 'reaction' || content.type === 'redaction'
        ? content.event
        : content.type === 'text'
          ? content.replaces
          : null
    const decided = item.wanted && item.state !== 'refused'
    if (target === null || !decided || !loaded.has(target)) {
      outgoing.push(item)
      continue
    }
    if (content.type === 'redaction') overlays.redacted.add(target)
    else overlays.of(target).push(item)
  }
  return [overlays, outgoing]
}

/** Of each loaded event, the sends going out that change it. */
class Overlays {
  private byTarget = new Map<string, MatrixSend[]>()
  /** Events, and reactions, being deleted. */
  readonly redacted = new Set<string>()

  of(target: string): MatrixSend[] {
    let overlay = this.byTarget.get(target)
    if (!overlay) {
      overlay = []
      this.byTarget.set(target, overlay)
    }
    return overlay
  }

  get(target: string): Overlay {
    return this.byTarget.get(target) ?? NONE
  }
}

const NONE: Overlay = []

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
        <button type="button" onMouseDown={keepFocus} onClick={change('leave')}>
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
  const last = events?.at(-1)
  // What was marked, so that a room list reloaded meanwhile does not
  // mark it again.
  const marked = useRef<number | null>(null)
  const read = room.read_up_to?.pos
  useEffect(() => {
    if (!visible || last === undefined || (read !== undefined && last.pos <= read) || marked.current === last.id) return
    marked.current = last.id
    matrix.markRead(room.id, last.id)
  }, [visible, last, read, room.id, matrix])
}

function Pending(props: { item: MatrixSend }) {
  const { item } = props
  return (
    <li class={`${styles.row} ${styles.pending}`}>
      <span class={styles.time} />
      <span class={styles.sender} />
      <div class={styles.content}>
        <span class={styles.body}>{sendSummary(item.content)}</span>{' '}
        <span class={styles.muted}>{matrixSendState(item)}</span>
        <MatrixSendActions item={item} class={styles.cancel} />
      </div>
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
