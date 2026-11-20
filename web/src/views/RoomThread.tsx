// A thread beside its room's timeline (SPEC §12.11), as Element's thread
// panel: its root, its messages, those being sent to it, and a box to
// write to it.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks'
import type { Room, RoomEvent, RoomEventId } from '../generated/protocol'
import { useVisible } from '../marks'
import { useServices } from '../context'
import { focusShown, keepFocus, pages, useKeymap } from '../keymap'
import { TEXT, shown } from '../matrix'
import { useStops } from '../regions'
import { Events, LINE, NO_READERS_BY_EVENT, type ThreadOpened, moveLine, overlaid, rows } from './Room'
import { Composer, type Mode, useMode } from './RoomComposer'
import styles from './Room.module.css'
import { SidePanel } from './SidePanel'

export function ThreadPanel(props: {
  room: Room
  opened: ThreadOpened
  me: string
  close: () => void
  openMember: (user: string) => void
}) {
  const { room, me } = props
  const { matrix } = useServices()
  const { opened } = props
  const { root } = opened
  const thread = matrix.thread(room.id, root)
  const state = thread.state.value
  const sends = matrix.timeline(room.id).outgoing.value
  const composer = useRef<HTMLTextAreaElement>(null)
  const scroller = useRef<HTMLDivElement>(null)
  const listed = useRef<HTMLOListElement>(null)
  const at = useMemo(() => ({ room: room.id, thread: root }), [room.id, root])
  const [mode, setMode] = useMode(at)
  const events = useMemo(() => (state ? [state.root, ...state.replies] : []), [state])
  const rootId = state?.root.event_id ?? null
  const [overlay, outgoing] = useMemo(() => overlaid(events, sends, rootId), [events, sends, rootId])
  const list = useMemo(() => rows(events, null, me), [events, me])
  const own = useMemo(
    () =>
      state?.replies.filter(
        (e) => e.sender === me && e.kind === 'm.room.message' && !e.redacted && TEXT.has(shown(e).msgtype ?? ''),
      ) ?? [],
    [state, me],
  )
  const act = (next: Mode) => {
    setMode(next)
    composer.current?.focus()
  }
  // Opened, again too, what it was opened for has the focus, once there
  // is any of it; after its box takes it, shown.
  const loaded = state !== null
  useLayoutEffect(() => {
    if (!loaded) return
    if (opened.focus === 'first') listed.current!.querySelector<HTMLElement>('[data-event]')!.focus()
    else if (composer.current) focusShown(composer.current)
  }, [loaded, opened])
  // Its messages are one Tab stop: the line the focus was on, or else
  // the latest.
  useStops(
    scroller,
    LINE,
    (last) => last?.closest(LINE) ?? [...listed.current!.querySelectorAll(LINE)].at(-1) ?? null,
    null,
  )
  // From its messages, its own box is written in, not the room's.
  useKeymap({ write: () => composer.current?.focus() }, { focus: listed })
  const reading = useThreadReading(room, root, state, me, listed, scroller)
  return (
    <SidePanel
      class={styles.thread}
      label="Thread"
      title="Thread"
      stops="[data-event]"
      back={null}
      up={reading.unread.length > 0 ? reading.markRead : null}
      close={props.close}
      actions={
        state !== null &&
        room.membership === 'joined' &&
        matrix.follows(room) &&
        (!state.root.redacted || state.root.followed) && (
          <button
            type="button"
            class={styles.follow}
            onMouseDown={keepFocus}
            onClick={() => matrix.setFollowed(room.id, state.root.event_id, !state.root.followed)}
            aria-pressed={state.root.followed}
          >
            {state.root.followed ? 'Following' : 'Follow'}
          </button>
        )
      }
    >
      {thread.error.value !== null && (
        <p class={styles.error} role="alert">
          {thread.error.value}
        </p>
      )}
      {reading.unread.length > 0 && (
        <div class={styles.catchUp} role="toolbar" aria-label="Catching up on the thread">
          <span>{reading.unread.length === 1 ? '1 new reply' : `${reading.unread.length} new replies`}</span>
          <button type="button" class={styles.link} title="Mark as read (Esc)" onMouseDown={keepFocus} onClick={reading.markRead}>
            Mark as read
          </button>
        </div>
      )}
      <div ref={scroller} class={styles.scroller}>
        <ol ref={listed} class={styles.events} aria-label="Thread messages">
          {state === null && <li class={styles.muted}>Loading…</li>}
          <Events
            list={listed}
            room={room}
            me={me}
            rows={list}
            overlay={overlay}
            outgoing={outgoing}
            readers={NO_READERS_BY_EVENT}
            act={act}
            openThread={null}
            markUnreadFrom={null}
            details={null}
            openMember={props.openMember}
            target={mode.type === 'new' ? null : mode.event.id}
            closePanel={null}
          />
        </ol>
      </div>
      {room.membership === 'joined' && rootId !== null && matrix.drafts.drafts.value !== null && (
        <Composer
          room={room}
          at={at}
          textarea={composer}
          mode={mode}
          setMode={setMode}
          own={own}
          thread={rootId}
          scrolling={pages(scroller)}
          focusLine={(event) => listed.current!.querySelector<HTMLElement>(`[data-event="${event}"]`)?.focus()}
          back={() => moveLine(listed.current!, -1)}
        />
      )}
    </SidePanel>
  )
}

/** Reads the thread as its replies come wholly on screen, in a visible
 * tab, as the room's timeline is read (SPEC §12.15): its read marker
 * (MSC3771) follows the latest from others shown. Its replies are read
 * up to that, the room's read marker, or the account's own latest. */
function useThreadReading(
  room: Room,
  root: RoomEventId,
  state: { replies: readonly RoomEvent[]; read_up_to: { pos: number } | null } | null,
  me: string,
  list: { current: HTMLOListElement | null },
  scroller: { current: HTMLDivElement | null },
): { unread: readonly RoomEvent[]; markRead: () => void } {
  const { matrix } = useServices()
  const visible = useVisible()
  const onScreen = useRef(new Set<RoomEventId>())
  const [reported, setReported] = useState(0)
  const replies = state?.replies
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
  }, [visible, replies, list, scroller])
  const own = replies?.findLast((e) => e.sender === me)?.pos ?? -Infinity
  const at = Math.max(state?.read_up_to?.pos ?? -Infinity, room.read_up_to?.pos ?? -Infinity, own)
  const unread = (replies ?? []).filter((e) => e.pos > at && e.sender !== me && e.kind === 'm.room.message' && !e.redacted)
  // What was marked, so that a thread listed again meanwhile does not
  // mark it again.
  const marked = useRef<RoomEventId | null>(null)
  useEffect(() => {
    if (!visible) return
    const latest = unread.findLast((e) => onScreen.current.has(e.id))
    if (latest && marked.current !== latest.id) {
      marked.current = latest.id
      matrix.markThreadRead(room.id, root, latest.id, true)
    }
  }, [visible, unread, reported, matrix, room.id, root])
  return {
    unread,
    markRead: () => {
      const latest = replies?.at(-1)
      if (latest) matrix.markThreadRead(room.id, root, latest.id, false)
    },
  }
}
