// A thread beside its room's timeline (SPEC §12.11), as Element's thread
// panel: its root, its messages, those being sent to it, and a box to
// write to it.

import { useMemo, useRef, useState } from 'preact/hooks'
import type { Room, RoomEventId } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, pages } from '../keymap'
import { TEXT, shown } from '../matrix'
import { Events, NO_READERS_BY_EVENT, overlaid, rows } from './Room'
import { Composer, type Mode } from './RoomComposer'
import styles from './Room.module.css'
// Its header as the room info's.
import panel from './RoomInfo.module.css'

export function ThreadPanel(props: { room: Room; root: RoomEventId; me: string; close: () => void }) {
  const { room, me } = props
  const { matrix } = useServices()
  const thread = matrix.thread(room.id, props.root)
  const state = thread.state.value
  const sends = matrix.timeline(room.id).sends.value
  const composer = useRef<HTMLTextAreaElement>(null)
  const scroller = useRef<HTMLDivElement>(null)
  const lines = useRef<HTMLOListElement>(null)
  const [mode, setMode] = useState<Mode>({ type: 'new' })
  const events = useMemo(() => (state ? [state.root, ...state.replies] : []), [state])
  const rootId = state?.root.event_id ?? null
  const [overlay, outgoing] = useMemo(() => overlaid(events, sends ?? [], rootId), [events, sends, rootId])
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
        <ol ref={lines} class={styles.events} aria-label="Thread messages">
          {state === null && <li class={styles.muted}>Loading…</li>}
          <Events
            list={lines}
            room={room}
            me={me}
            rows={list}
            overlay={overlay}
            outgoing={outgoing}
            readers={NO_READERS_BY_EVENT}
            act={act}
            openThread={null}
          />
        </ol>
      </div>
      {room.membership === 'joined' && rootId !== null && (
        <Composer
          room={room}
          textarea={composer}
          mode={mode}
          setMode={setMode}
          lastOwn={lastOwn}
          thread={rootId}
          attachments={null}
          scrolling={pages(scroller)}
        />
      )}
    </aside>
  )
}
