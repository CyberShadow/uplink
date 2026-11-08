// The user's ops, newest first, each with its Undo (SPEC §6.3, §12.4).
// Undoing an undo is a redo.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { HistoryEntry } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './History.module.css'

export function History() {
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
    next: () => {
      setFocus((f) => Math.min(f + 1, count - 1))
      if (focus >= count - 10) history.loadMore()
    },
    previous: () => setFocus((f) => Math.max(f - 1, 0)),
    undo: () => current && current.undone_by === null && ops.undo(current.entry.id),
  })

  if (entries === null) return <p class={styles.placeholder}>Loading…</p>
  if (entries.length === 0) return <p class={styles.placeholder}>Nothing done yet.</p>
  return (
    <ul
      class={styles.list}
      aria-label="History"
      onScroll={(event) => {
        const el = event.currentTarget
        if (el.scrollTop + el.clientHeight > el.scrollHeight - 200) history.loadMore()
      }}
    >
      {entries.map((h, i) => (
        <Item
          key={h.entry.id}
          history={h}
          focused={i === focus}
          itemRef={i === focus ? focused : undefined}
          onFocus={() => setFocus(i)}
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
      class={[styles.item, props.focused && styles.focused, undone && styles.undone].filter(Boolean).join(' ')}
      aria-current={props.focused || undefined}
      onClick={props.onFocus}
    >
      <time class={styles.time} dateTime={new Date(entry.ts).toISOString()}>
        {new Date(entry.ts).toLocaleString(undefined, { dateStyle: 'short', timeStyle: 'short' })}
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
