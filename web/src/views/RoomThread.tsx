// A thread beside its room's timeline (SPEC §12.11), as Element's thread
// panel: its root, its messages, those being sent to it, and a box to
// write to it.

import { useLayoutEffect, useMemo, useRef } from 'preact/hooks'
import type { Room } from '../generated/protocol'
import { useServices } from '../context'
import { focusShown, keepFocus, pages } from '../keymap'
import { TEXT, shown } from '../matrix'
import { Events, NO_READERS_BY_EVENT, type ThreadOpened, moveLine, overlaid, rows } from './Room'
import { Composer, type Mode, useMode } from './RoomComposer'
import styles from './Room.module.css'
// Its header as the room info's.
import panel from './RoomInfo.module.css'

export function ThreadPanel(props: {
  room: Room
  opened: ThreadOpened
  me: string
  /** Gives the focus to the room's timeline: to its root's line. */
  toRoom: () => void
  /** Closes it, for the room's timeline. */
  leave: () => void
  /** Closes it, for the room's box. */
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
  const lastOwn = useMemo(
    () =>
      state?.replies.findLast(
        (e) => e.sender === me && e.kind === 'm.room.message' && !e.redacted && TEXT.has(shown(e).msgtype ?? ''),
      ) ?? null,
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
  return (
    <aside class={styles.thread} aria-label="Thread">
      <header class={panel.header}>
        <h2>Thread</h2>
        <button type="button" onMouseDown={keepFocus} onClick={props.close} aria-label="Close thread">
          ×
        </button>
      </header>
      {thread.error.value !== null && (
        <p class={styles.error} role="alert">
          {thread.error.value}
        </p>
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
            threadOpen={null}
            toRoom={props.toRoom}
            details={null}
            openMember={props.openMember}
            target={mode.type === 'new' ? null : mode.event.id}
            closePanel={props.leave}
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
          lastOwn={lastOwn}
          thread={rootId}
          scrolling={pages(scroller)}
          focusLine={(event) => listed.current!.querySelector<HTMLElement>(`[data-event="${event}"]`)?.focus()}
          back={() => moveLine(listed.current!, -1)}
        />
      )}
    </aside>
  )
}
