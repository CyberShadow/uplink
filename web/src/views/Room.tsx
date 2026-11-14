// A Matrix room (SPEC §9, §12.11), laid out as IRC clients do, and as
// Element's IRC layout: a line per event, its time, sender and body side
// by side; days apart; a line where the unread begin. Messages being
// sent follow, and the box to write one. Beside it, a panel: the room's
// info, a member's, or a thread.

import type { RefObject } from 'preact'
import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useVisible } from '../marks'
import { useLocation } from 'preact-iso'
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
import { focusShown, keepFocus, lines, pages, useKeymap } from '../keymap'
import styles from './Room.module.css'
import { RoomAvatar } from './RoomAvatar'
import { date, dateTime, time } from '../format'
import { TEXT, displayName, sendSummary, senderColor, shown } from '../matrix'
import { mention } from './Autocomplete'
import { EventRow, type Line, type LineActions, lineActions, MembershipRun, type Reacting, reveal, type Overlay } from './RoomEvent'
import { Badge } from './Badge'
import { Icon } from './Icon'
import { useLoadInView } from './loadInView'
import { RoomSearch } from './RoomSearch'
import { Composer, type Mode } from './RoomComposer'
import { MemberPanel, RoomInfo } from './RoomInfo'
import { MessageDetails } from './RoomMessage'
import { ThreadPanel } from './RoomThread'
import { RoomTagPicker } from './RoomTagPicker'
import { tagName } from '../roomTags'
import { useTitle } from '../title'

export function roomPath(room: number): string {
  return `/rooms/${room}`
}

/** The room, its messages searched for `words`. */
export function roomSearchPath(room: number, words: string): string {
  return `${roomPath(room)}/search/${encodeURIComponent(words)}`
}

export function RoomView(props: { room?: string; words?: string }) {
  const { matrix } = useServices()
  const rooms = matrix.rooms.value
  const room = rooms?.find((r) => String(r.id) === props.room)
  useTitle(...(room ? [room.name] : []))
  if (!rooms) return <p class={styles.placeholder}>Loading…</p>
  if (!room) return <p class={styles.placeholder}>No such room.</p>
  return (
    <Timeline key={room.id} room={room} words={props.words === undefined ? null : decodeURIComponent(props.words)} />
  )
}

/** `words`: those its messages are searched for, if any. */
function Timeline(props: { room: Room; words: string | null }) {
  const { room, words } = props
  const { route } = useLocation()
  const { matrix } = useServices()
  const timeline = matrix.timeline(room.id)
  const events = timeline.events.value
  const sends = timeline.sends.value
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const composer = useRef<HTMLTextAreaElement>(null)
  const [mode, setMode] = useState<Mode>({ type: 'new' })
  // Where catching up was when the room was opened: reading moves the
  // marker, but not the line, while it is on show (SPEC §12.15).
  const [caughtUp] = useState(room.fully_read)
  const [files, setFiles] = useState<File[]>([])
  // A search shows over another panel, which closing it shows again.
  const [opened, setOpened] = useState<Panel>(null)
  const panel: Panel = words !== null ? { type: 'search', words } : opened
  const setPanel = (panel: Panel) => {
    if (words !== null) route(roomPath(room.id))
    setOpened(panel)
  }
  const [tagging, setTagging] = useState(false)
  const write = () => composer.current?.focus()
  const openMember = (user: string) => setPanel({ type: 'member', user, back: false })
  const scroller = useRef<HTMLDivElement>(null)
  const scrolling = pages(scroller)
  const listed = useRef<HTMLOListElement>(null)
  // From no line (those of a list have its own), to the latest.
  const toLatest = () => moveLine(listed.current!, -1)
  const focusLine = (event: RoomEventId) => lineOf(listed.current!, event)?.focus()
  const reading = useReading(room, events, me, listed, scroller)
  const behind = reading.behind.length > 0 || reading.more
  useKeymap({
    reply: write,
    write,
    label: () => setTagging(true),
    ...lines(toLatest, toLatest),
    ...scrolling,
    jumpToUnread: reading.jumpToFirstUnread,
    nextMention: reading.nextMention,
    // Else, home (`Shell`).
    ...(panel ? { up: () => setPanel(null) } : behind && { up: reading.markRead }),
  })

  const [overlay, outgoing] = useMemo(() => overlaid(events ?? [], sends ?? [], null), [events, sends])
  const receipts = timeline.receipts.value
  const readers = useMemo(() => readersByEvent(events ?? [], receipts), [events, receipts])
  const gaps = timeline.gaps.value
  const list = useMemo(() => rows(events ?? [], caughtUp, me, gaps), [events, caughtUp, me, gaps])
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
          <button
            type="button"
            class={styles.tags}
            aria-label="Tags"
            title="Tags (l)"
            onMouseDown={keepFocus}
            onClick={() => setTagging(true)}
          >
            {room.tags.map((tag) => (
              <span key={tag} class={styles.chip}>
                {tagName(tag)}
              </span>
            ))}
            <Icon name="tag" />
          </button>
          <span class={styles.muted}>
            <button
              type="button"
              class={styles.link}
              aria-expanded={panel?.type === 'info'}
              onMouseDown={keepFocus}
              onClick={() => setPanel(panel?.type === 'info' ? null : { type: 'info' })}
            >
              {room.joined_members === 1 ? '1 member' : `${room.joined_members} members`}
            </button>
            {room.encrypted && ' · encrypted'}
          </span>
          <MembershipControls room={room} />
        </header>
        {room.membership_error !== null && (
          <p class={styles.error} role="alert">
            The homeserver refused: {room.membership_error}. <a href="/history">Undo the change</a> to keep the room
            as it is.
          </p>
        )}
        {room.pinned.length > 0 && <PinnedBar room={room} />}
        <CatchUp reading={reading} />
        {/* Reversed, so that it stays scrolled to the latest event. */}
        <div ref={scroller} class={styles.scroller}>
          <ol
            ref={listed}
            class={styles.events}
            aria-label="Timeline"
            // Open, a message's details show the line that has the focus.
            onFocusIn={(event) => {
              const line = (event.target as Element).closest<HTMLElement>('[data-event]')
              if (panel?.type === 'message' && line) setPanel({ type: 'message', event: Number(line.dataset.event) })
            }}
          >
            {timeline.moreBefore.value && <Earlier room={room} />}
            {events === null && <li class={styles.muted}>Loading…</li>}
            <Events
              list={listed}
              room={room}
              me={me}
              rows={list}
              overlay={overlay}
              outgoing={outgoing}
              readers={readers}
              act={act}
              openThread={(root, back) => setPanel({ type: 'thread', root, back })}
              details={(event) => setPanel({ type: 'message', event })}
              openMember={openMember}
              target={mode.type === 'new' ? null : mode.event.id}
              closePanel={panel && (() => setPanel(null))}
            />
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
            thread={null}
            attachments={{ files, setFiles }}
            scrolling={scrolling}
            focusLine={focusLine}
            back={null}
          />
        )}
      </section>
      {panel?.type === 'info' && (
        <RoomInfo
          room={room}
          close={() => setPanel(null)}
          openMember={(user) => setPanel({ type: 'member', user, back: true })}
        />
      )}
      {panel?.type === 'member' && (
        <MemberPanel
          key={panel.user}
          room={room}
          user={panel.user}
          back={panel.back ? () => setPanel({ type: 'info' }) : null}
          close={() => setPanel(null)}
          mention={(user, name) => {
            // As if typed where the caret is.
            const box = composer.current!
            box.focus()
            box.setRangeText(`${mention(user, name)} `, box.selectionStart, box.selectionEnd, 'end')
            box.dispatchEvent(new Event('input', { bubbles: true }))
          }}
        />
      )}
      {panel?.type === 'search' && (
        <RoomSearch
          key={panel.words}
          room={room}
          words={panel.words}
          close={() => route(roomPath(room.id))}
          open={async (event) => {
            if (event.thread_root !== null) {
              setPanel({ type: 'thread', root: event.thread_root, back: false })
              return
            }
            await timeline.loadUntil(event.pos)
            // Once rendered.
            requestAnimationFrame(() => reveal(event.id))
          }}
        />
      )}
      {panel?.type === 'thread' && (
        <ThreadPanel
          key={panel.root}
          room={room}
          root={panel.root}
          me={me}
          back={panel.back ? () => focusLine(panel.root) : null}
          close={() => {
            setPanel(null)
            // From the thread's box, back to the room's.
            if (composer.current) focusShown(composer.current)
          }}
          openMember={openMember}
        />
      )}
      {panel?.type === 'message' && <Details room={room} events={events} readers={readers} id={panel.event} close={() => setPanel(null)} />}
      {tagging && <RoomTagPicker rooms={[room]} close={() => setTagging(false)} />}
    </div>
  )
}

/** A message's details, while it is in the timeline. */
function Details(props: {
  room: Room
  events: readonly RoomEvent[] | null
  readers: ReadonlyMap<RoomEventId, readonly MemberReceipt[]>
  id: RoomEventId
  close: () => void
}) {
  const event = props.events?.find((e) => e.id === props.id)
  if (!event) return null
  return (
    <MessageDetails room={props.room} event={event} readers={props.readers.get(event.id) ?? NO_READERS} close={props.close} />
  )
}

/** What shows beside the timeline, if anything. */
type Panel =
  | null
  | { type: 'info' }
  // `back`: opened from the room's info, to return to.
  | { type: 'member'; user: string; back: boolean }
  // `back`: opened by a key on its root's line, which Esc from its box
  // goes back to.
  | { type: 'thread'; root: RoomEventId; back: boolean }
  | { type: 'search'; words: string }
  | { type: 'message'; event: RoomEventId }

/** The rows of a timeline, or a thread, and the sends that follow; and,
 * while one of its lines has the focus, the keys that act on its event
 * (`lineActions`), and `j`/`k`, which move the focus to the next line. */
export function Events(props: {
  room: Room
  me: string
  /** The list the rows are of. */
  list: RefObject<HTMLOListElement>
  rows: readonly Row[]
  overlay: Overlays
  outgoing: readonly MatrixSend[]
  readers: ReadonlyMap<RoomEventId, readonly MemberReceipt[]>
  act: (mode: Mode) => void
  /** Of the main timeline, opens a thread by its root, and whether by a
   * key on its line. */
  openThread: ((root: RoomEventId, back: boolean) => void) | null
  /** Of the main timeline, shows a message's details. */
  details: ((event: RoomEventId) => void) | null
  /** Opens a member's profile, by their user id. */
  openMember: (user: string) => void
  /** The event replied to or edited, if any, which its line shows. */
  target: RoomEventId | null
  /** Of the main timeline, closes the panel beside it, if one: what Esc
   * does first, from a line, keeping it. */
  closePanel: (() => void) | null
}) {
  const { room, me, overlay, act, openThread, details, openMember, closePanel } = props
  const { matrix } = useServices()
  const [reacting, setReacting] = useState<{ event: RoomEventId; how: Reacting } | null>(null)
  // The line whose deleting is being asked about.
  const [confirming, setConfirming] = useState<RoomEventId | null>(null)
  /** A line's event, and what it does: by a key on it, which Esc goes
   * back to (`back`), or by its buttons. */
  const line = (event: RoomEvent, back: boolean): Line => ({
    room,
    event,
    me,
    overlay: overlay.get(event.event_id),
    pendingRedactions: overlay.redacted,
    reply: () => act({ type: 'reply', event, back }),
    edit: () => act({ type: 'edit', event, back }),
    openThread: openThread && (() => openThread(event.id, back)),
    openMember,
  })
  /** What keys can do to the event of the line that has the focus. */
  const focused = (): LineActions & { id: RoomEventId } => {
    const id = Number((document.activeElement as Element).closest<HTMLElement>('[data-event]')?.dataset.event)
    const row = props.rows.find((r) => r.type === 'event' && r.event.id === id)
    // The focus may be in the list, but on none of its lines: on a
    // button loading more, say.
    return row?.type === 'event' ? { id, ...lineActions(line(row.event, true), matrix) } : { id }
  }
  useKeymap(
    {
      ...lines(
        () => moveLine(props.list.current!, 1),
        () => moveLine(props.list.current!, -1),
      ),
      reply: () => focused().reply?.(),
      edit: () => focused().edit?.(),
      thread: () => {
        const can = focused()
        ;(can.thread ?? can.replyInThread)?.()
      },
      react: () => {
        const can = focused()
        if (can.react) setReacting({ event: can.id, how: 'key' })
      },
      delete: () => {
        const can = focused()
        if (can.delete) setConfirming(can.id)
      },
      pin: () => focused().pin?.toggle(),
      ...(closePanel && { up: closePanel }),
      ...(details && {
        open: () => {
          const { id } = focused()
          if (!Number.isNaN(id)) details(id)
        },
      }),
    },
    { focus: props.list },
  )
  return (
    <>
      {props.rows.map((row) => {
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
          case 'gap':
            return <Gap key={row.key} room={room} gap={row.gap} />
          case 'event':
            return (
              <EventRow
                key={row.key}
                {...line(row.event, false)}
                target={props.target === row.event.id}
                readers={props.readers.get(row.event.id) ?? NO_READERS}
                reacting={reacting?.event === row.event.id ? reacting.how : null}
                setReacting={(how) => setReacting(how && { event: row.event.id, how })}
                confirming={confirming === row.event.id}
                setConfirming={(asking) => setConfirming(asking ? row.event.id : null)}
              />
            )
        }
      })}
      {props.outgoing.map((item) => (
        <Pending key={item.id} item={item} />
      ))}
    </>
  )
}

/** The line of `event` in `list`, if shown. */
function lineOf(list: HTMLElement, event: RoomEventId): HTMLElement | null {
  return list.querySelector<HTMLElement>(`[data-event="${event}"]`)
}

/** Moves the focus to the line `by` after the one that has it in `list`,
 * or else to its last, the latest. */
export function moveLine(list: HTMLElement, by: 1 | -1) {
  const lines = [...list.querySelectorAll<HTMLElement>('[data-event]')]
  const at = lines.findIndex((l) => l.contains(document.activeElement))
  const to = at === -1 ? lines.length - 1 : Math.max(0, Math.min(at + by, lines.length - 1))
  lines[to]?.focus()
}

/** The start of the events loaded, with more before: those load as it
 * comes into view. */
function Earlier(props: { room: Room }) {
  const { matrix } = useServices()
  const timeline = matrix.timeline(props.room.id)
  const element = useRef<HTMLLIElement>(null)
  useLoadInView(element, timeline.events.value?.[0]?.id, () => timeline.loadEarlier())
  return (
    <li ref={element} class={styles.earlier}>
      <button type="button" onMouseDown={keepFocus} onClick={() => timeline.loadEarlier()}>
        Load earlier events
      </button>
    </li>
  )
}

/** Where events are missing: the homeserver is asked for them as it
 * comes into view. Those fetched fill it, and a gap left where they end
 * is another. */
function Gap(props: { room: Room; gap: GapId }) {
  const { matrix } = useServices()
  const timeline = matrix.timeline(props.room.id)
  const element = useRef<HTMLLIElement>(null)
  useLoadInView(element, props.gap, () => timeline.fill(props.gap))
  const filling = timeline.filling.value.has(props.gap)
  return (
    <li ref={element} class={styles.gap} role="separator" aria-label="Events missing">
      <span>Earlier events are missing here.</span>
      <button type="button" disabled={filling} onMouseDown={keepFocus} onClick={() => timeline.fill(props.gap)}>
        {filling ? 'Fetching…' : 'Fetch them'}
      </button>
    </li>
  )
}

/** The room's pinned messages, as Element's banner: one at a time, the
 * latest pinned first; a click shows it in the timeline, and the next. */
function PinnedBar(props: { room: Room }) {
  const { room } = props
  const { matrix } = useServices()
  const pinned = matrix.pinned(room.id).events.value
  const [at, setAt] = useState(0)
  if (pinned === null) return null
  // Newest first; of those stored.
  const events = [...pinned].reverse()
  if (events.length === 0) {
    return (
      <p class={`${styles.pinned} ${styles.muted}`}>
        <Icon name="pin" />
        {room.pinned.length === 1 ? 'A message is pinned' : `${room.pinned.length} messages are pinned`}, not fetched
        yet.
      </p>
    )
  }
  const index = at % events.length
  const event = events[index]
  return (
    <button
      type="button"
      class={styles.pinned}
      aria-label="Pinned messages"
      onMouseDown={keepFocus}
      onClick={async () => {
        setAt(index + 1)
        await matrix.timeline(room.id).loadUntil(event.pos)
        requestAnimationFrame(() => reveal(event.id))
      }}
    >
      <Icon name="pin" />
      <span class={styles.muted}>
        {events.length === 1 ? 'Pinned message' : `Pinned message ${index + 1} of ${events.length}`}
      </span>
      <span class={styles.quoted}>
        <span style={{ color: senderColor(event.sender) }}>{displayName(event.sender, event.sender_name)}</span>:{' '}
        {event.redacted ? 'Message deleted' : (shown(event).body ?? event.kind)}
      </span>
    </button>
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
export const NO_READERS_BY_EVENT: ReadonlyMap<RoomEventId, readonly MemberReceipt[]> = new Map()

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

export type Row =
  | { type: 'day'; key: string; date: string }
  | { type: 'unread'; key: string }
  | { type: 'event'; key: string; event: RoomEvent }
  | { type: 'members'; key: string; events: RoomEvent[] }
  | { type: 'gap'; key: string; gap: GapId }

/** Membership changes in a row that are folded into one, as Element
 * folds them: at least these many. */
const RUN = 3

/** The timeline's rows: a day's line before its first event, the "New"
 * line before the first event from others after `readUpTo` (the
 * fully-read marker, SPEC §12.15), runs of
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
 * listed with the room's other sends to `thread` (a root's Matrix id, or
 * `null`: the main timeline), at the end, with their buttons. */
export function overlaid(
  events: readonly RoomEvent[],
  sends: readonly MatrixSend[],
  thread: string | null,
): [Overlays, MatrixSend[]] {
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
      if ((content.type === 'text' ? content.thread : null) === thread) outgoing.push(item)
      continue
    }
    if (content.type === 'redaction') overlays.redacted.add(target)
    else overlays.of(target).push(item)
  }
  return [overlays, outgoing]
}

/** Of each loaded event, the sends going out that change it. */
export class Overlays {
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

/** What reading the room has got to (SPEC §12.15), and its ways in. */
interface Reading {
  /** Messages from others after the fully-read marker, loaded. */
  behind: readonly RoomEvent[]
  /** Whether there may be more before them, not loaded. */
  more: boolean
  /** Of those, the mentions not yet on screen. */
  mentions: readonly RoomEvent[]
  /** Done with it: read and caught up on, to its latest. */
  markRead: () => void
  jumpToFirstUnread: () => void
  /** To the oldest mention not yet on screen. */
  nextMention: () => void
}

/** Reads the room as its messages come wholly on screen, in a visible
 * tab, as Element's receipts do: the read marker follows the latest from
 * others shown, and the fully-read marker moves over what was shown
 * without a gap after it (SPEC §12.15). */
function useReading(
  room: Room,
  events: readonly RoomEvent[] | null,
  me: string,
  list: RefObject<HTMLOListElement>,
  scroller: RefObject<HTMLDivElement>,
): Reading {
  const { matrix } = useServices()
  const timeline = matrix.timeline(room.id)
  const visible = useVisible()
  // The events wholly on screen while the room has shown; reported as
  // they come, so rendered anew.
  const onScreen = useRef(new Set<RoomEventId>())
  const [reported, setReported] = useState(0)
  useEffect(() => {
    if (!visible) return
    const observer = new IntersectionObserver(
      (entries) => {
        const shown = entries.filter((e) => e.isIntersecting && e.intersectionRatio >= 1)
        for (const entry of shown) onScreen.current.add(Number((entry.target as HTMLElement).dataset.event))
        if (shown.length > 0) setReported((n) => n + 1)
      },
      { root: scroller.current, threshold: 1 },
    )
    for (const line of list.current!.querySelectorAll('[data-event]')) observer.observe(line)
    return () => observer.disconnect()
  }, [visible, events, list, scroller])

  // What was marked, so that a room list reloaded meanwhile does not
  // mark it again.
  const marked = useRef<RoomEventId | null>(null)
  const caught = useRef<RoomEventId | null>(null)
  const gaps = timeline.gaps.value
  const moreBefore = timeline.moreBefore.value
  const read = room.read_up_to?.pos
  const fully = room.fully_read
  useEffect(() => {
    if (!visible || events === null) return
    const seen = onScreen.current
    const latest = events.findLast((e) => e.sender !== me && seen.has(e.id))
    if (latest && (read === undefined || latest.pos > read) && marked.current !== latest.id) {
      marked.current = latest.id
      matrix.markRead(room.id, latest.id, true, false)
    }
    // From the marker, if loaded (or, without one, the room's start),
    // over what was shown, the account's own, and what has no line,
    // until a gap or a line not yet shown.
    const from = fully === null ? (moreBefore ? -1 : 0) : events.findIndex((e) => e.id === fully.event) + 1
    if (from <= 0 && (fully !== null || from < 0)) return
    const lines = new Set([...list.current!.querySelectorAll<HTMLElement>('[data-event]')].map((l) => Number(l.dataset.event)))
    const gapBefore = new Set(gaps.flatMap((g) => (g.before === null ? [] : [g.before])))
    let to: RoomEvent | null = null
    for (const event of events.slice(from)) {
      if (gapBefore.has(event.id)) break
      if (lines.has(event.id) && event.sender !== me && !seen.has(event.id)) break
      to = event
    }
    if (to && caught.current !== to.id) {
      caught.current = to.id
      matrix.markFullyRead(room.id, to.id)
    }
  }, [visible, events, reported, read, fully, moreBefore, gaps, me, room.id, matrix, list])

  const at = fully === null ? -Infinity : fully.pos
  const behind = (events ?? []).filter(
    (e) => e.pos > at && e.sender !== me && e.kind === 'm.room.message' && !e.redacted && e.thread_root === null,
  )
  const loaded = fully === null ? !moreBefore : (events ?? []).some((e) => e.id === fully.event)
  const mentions = behind.filter((e) => e.highlight && !onScreen.current.has(e.id))
  const show = async (pos: number, id: () => RoomEventId | undefined) => {
    await timeline.loadUntil(pos)
    // Once rendered.
    requestAnimationFrame(() => {
      const event = id()
      if (event !== undefined) reveal(event)
    })
  }
  return {
    behind,
    more: !loaded,
    mentions,
    markRead: () => {
      const latest = room.latest ?? events?.at(-1)?.id
      if (latest !== undefined) matrix.markRead(room.id, latest, false, true)
    },
    jumpToFirstUnread: () => {
      void show(at, () => timeline.events.value?.find((e) => e.pos > at && e.sender !== me)?.id)
    },
    nextMention: () => {
      const next = mentions[0]
      if (next) void show(next.pos, () => next.id)
    },
  }
}

/** While catching up is behind what shows: how many messages are after
 * the fully-read marker, and the mentions of them not yet shown, with
 * the ways to them, as Element's and Telegram's bars. */
function CatchUp(props: { reading: Reading }) {
  const { behind, more, mentions, markRead, jumpToFirstUnread, nextMention } = props.reading
  if (behind.length === 0 && !more) return null
  const count = `${behind.length}${more ? '+' : ''}`
  return (
    <div class={styles.catchUp} role="toolbar" aria-label="Catching up">
      <button type="button" class={styles.link} title="Jump to first unread (Shift+PgUp)" onMouseDown={keepFocus} onClick={jumpToFirstUnread}>
        {count === '1' ? '1 new message' : `${count} new messages`}
      </button>
      {mentions.length > 0 && (
        <button type="button" class={styles.mentions} title="Jump to the mention (Shift+PgDn)" onMouseDown={keepFocus} onClick={nextMention}>
          <Badge count={mentions.length} tone="alert" prefix="@" title="Mentioning you" />
        </button>
      )}
      <button type="button" class={styles.link} title="Mark as read (Esc)" onMouseDown={keepFocus} onClick={markRead}>
        Mark as read
      </button>
    </div>
  )
}

/** A message being sent, at the end of the timeline as it will be once
 * synced back. Going out as wanted, it says so by a spinner where its
 * time will be, and has its time once sent; otherwise it says, in words,
 * what is to be done about it. */
function Pending(props: { item: MatrixSend }) {
  const { item } = props
  const state = matrixSendState(item)
  const going = item.wanted && (item.state === 'queued' || item.state === 'sending')
  const quiet = item.wanted && item.state !== 'refused'
  return (
    <li class={[styles.row, going && styles.sending, !quiet && styles.pending].filter(Boolean).join(' ')}>
      {going ? (
        <span class={styles.time}>
          <span class={styles.spinner} role="img" aria-label={state} title={state} />
        </span>
      ) : (
        <time class={styles.time} dateTime={new Date(item.queued_at).toISOString()} title={dateTime(item.queued_at)}>
          {time(item.queued_at)}
        </time>
      )}
      <span class={styles.sender} />
      <div class={styles.content}>
        <span class={styles.body}>{sendSummary(item.content)}</span>
        {!quiet && <span class={styles.muted}> {state}</span>}
      </div>
      <span class={styles.sendActions}>
        <MatrixSendActions item={item} compact />
      </span>
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
      if (item.wanted) return 'Sent'
      if (item.error !== null) return `Could not undo: ${item.error}`
      return item.retractable ? 'Undoing…' : 'Cannot undo (already sent)'
    case 'refused':
      return `Not sent: ${item.error}`
    case 'taking_back':
      return 'Undoing…'
  }
}

/** Cancelling a message being sent, or accepting that one sent could not
 * be undone (keeping it sent). */
export function MatrixSendActions(props: { item: MatrixSend; compact?: boolean }) {
  const { item } = props
  const { matrix } = useServices()
  if (item.wanted && item.state !== 'sent') {
    // Compact, in its line, as an ×, shown on hover (Room.module.css).
    return props.compact ? (
      <button
        type="button"
        class={styles.cancel}
        aria-label="Cancel"
        title="Cancel sending"
        onMouseDown={keepFocus}
        onClick={() => matrix.cancelSend(item.id)}
      >
        <Icon name="x" />
      </button>
    ) : (
      <button type="button" onMouseDown={keepFocus} onClick={() => matrix.cancelSend(item.id)}>
        Cancel
      </button>
    )
  }
  if (!item.wanted && item.state === 'sent' && (!item.retractable || item.error !== null)) {
    return (
      <button type="button" onMouseDown={keepFocus} onClick={() => matrix.keepSent(item.id)}>
        OK
      </button>
    )
  }
  return null
}
