// The user's ops, newest first, each with its Undo (SPEC §6.3, §12.4),
// below what of them is not yet at the providers: the outbox (§6.6).
// Undoing an undo is a redo.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { HistoryEntry } from '../generated/protocol'
import { useServices } from '../context'
import { useTitle } from '../title'
import { keepFocus, lines, useKeymap } from '../keymap'
import styles from './History.module.css'
import { dateTime } from '../format'
import { Outbox } from './Outgoing'

export function History() {
  useTitle('History')
  const { history, ops } = useServices()
  const entries = history.entries.value
  const [focus, setFocus] = useState(0)
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [focus])
  const count = entries?.length ?? 0

  const current = entries?.[focus]
  useKeymap({
    ...lines(
      () => {
        setFocus((f) => Math.min(f + 1, count - 1))
        if (focus >= count - 10) history.loadMore()
      },
      () => setFocus((f) => Math.max(f - 1, 0)),
    ),
    undo: () => current && current.undone_by === null && ops.undo(current.entry.id),
  })

  return (
    <div
      class={styles.page}
      onScroll={(event) => {
        const el = event.currentTarget
        if (el.scrollTop + el.clientHeight > el.scrollHeight - 200) history.loadMore()
      }}
    >
      <section aria-label="Outbox">
        <h2 class={styles.heading}>Outbox</h2>
        <Outbox />
      </section>
      <section aria-label="History">
        <h2 class={styles.heading}>History</h2>
        <Entries entries={entries} focus={focus} focused={focused} setFocus={setFocus} />
      </section>
    </div>
  )
}

function Entries(props: {
  entries: readonly HistoryEntry[] | null
  focus: number
  focused: preact.Ref<HTMLLIElement>
  setFocus: (i: number) => void
}) {
  const { entries } = props
  if (entries === null) return <p class={styles.placeholder}>Loading…</p>
  if (entries.length === 0) return <p class={styles.placeholder}>Nothing done yet.</p>
  return (
    <ul class={styles.list} aria-label="History">
      {entries.map((h, i) => (
        <Item
          key={h.entry.id}
          history={h}
          focused={i === props.focus}
          itemRef={i === props.focus ? props.focused : undefined}
          onFocus={() => props.setFocus(i)}
        />
      ))}
    </ul>
  )
}

function Item(props: {
  history: HistoryEntry
  focused: boolean
  itemRef: preact.Ref<HTMLLIElement> | undefined
  onFocus: () => void
}) {
  const { ops } = useServices()
  const { entry, undone_by } = props.history
  const undone = undone_by !== null
  return (
    <li
      ref={props.itemRef}
      class={[styles.item, props.focused && styles.focused, undone && styles.undone, entry.shown && styles.shown]
        .filter(Boolean)
        .join(' ')}
      // Done by a view being shown, which `z` passes over (SPEC §12.15).
      title={entry.shown ? 'Done by showing it' : undefined}
      aria-current={props.focused || undefined}
      onClick={props.onFocus}
    >
      <time class={styles.time} dateTime={new Date(entry.ts).toISOString()}>
        {dateTime(entry.ts)}
      </time>
      <span class={styles.description}>{entry.description}</span>
      {undone ? (
        <span class={styles.state}>undone</span>
      ) : (
        <button type="button" onMouseDown={keepFocus} onClick={() => ops.undo(entry.id)}>
          {entry.undoes === null ? 'Undo' : 'Redo'}
        </button>
      )}
    </li>
  )
}
