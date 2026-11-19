// The user's ops, newest first, each with its Undo (SPEC §6.3, §12.4),
// below what of them is not yet at the providers: the outbox (§6.6).
// Undoing an undo is a redo. Edits of one text that follow each other
// (a draft being typed) are listed as one, which opens (§7.3).

import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import type { HistoryEntry, TxLogId } from '../generated/protocol'
import { useServices } from '../context'
import { runs } from '../stores/history'
import { useTitle } from '../title'
import { keepFocus, lines, useKeymap } from '../keymap'
import styles from './History.module.css'
import { dateTime } from '../format'
import { Outbox } from './Outgoing'

/** A line of the list: an entry, alone or of a run that is open; or a
 * run of them, of which the entries follow if it is open. */
type Line =
  | { type: 'entry'; history: HistoryEntry; inRun: boolean }
  | { type: 'run'; run: readonly HistoryEntry[]; open: boolean }

/** A run is known by its oldest entry, which stays as it grows. */
const runId = (run: readonly HistoryEntry[]): TxLogId => run[run.length - 1].entry.id

export function History() {
  useTitle('History')
  const { history, ops } = useServices()
  const entries = history.entries.value
  // The runs opened.
  const [opened, setOpened] = useState<ReadonlySet<TxLogId>>(new Set())
  const listed = useMemo(
    () =>
      entries &&
      runs(entries).flatMap((run): Line[] => {
        if (run.length === 1) return [{ type: 'entry', history: run[0], inRun: false }]
        const open = opened.has(runId(run))
        const within: Line[] = open ? run.map((history) => ({ type: 'entry', history, inRun: true })) : []
        return [{ type: 'run', run, open }, ...within]
      }),
    [entries, opened],
  )
  const toggle = (run: readonly HistoryEntry[]) => {
    const next = new Set(opened)
    if (!next.delete(runId(run))) next.add(runId(run))
    setOpened(next)
  }
  const [focus, setFocus] = useState(0)
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [focus])
  const count = listed?.length ?? 0

  const current = listed?.[focus]
  useKeymap({
    ...lines(
      () => {
        setFocus((f) => Math.min(f + 1, count - 1))
        if (focus >= count - 10) history.loadMore()
      },
      () => setFocus((f) => Math.max(f - 1, 0)),
    ),
    ...(current?.type === 'run' && { open: () => toggle(current.run) }),
    undo: () => current?.type === 'entry' && current.history.undone_by === null && ops.undo(current.history.entry.id),
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
        {listed === null ? (
          <p class={styles.placeholder}>Loading…</p>
        ) : listed.length === 0 ? (
          <p class={styles.placeholder}>Nothing done yet.</p>
        ) : (
          <ul class={styles.list} aria-label="History">
            {listed.map((line, i) => {
              const at = { focused: i === focus, itemRef: i === focus ? focused : undefined, onFocus: () => setFocus(i) }
              return line.type === 'entry' ? (
                <Item key={line.history.entry.id} history={line.history} inRun={line.inRun} {...at} />
              ) : (
                <Run key={`run ${runId(line.run)}`} run={line.run} open={line.open} toggle={() => toggle(line.run)} {...at} />
              )
            })}
          </ul>
        )}
      </section>
    </div>
  )
}

interface At {
  focused: boolean
  itemRef: preact.Ref<HTMLLIElement> | undefined
  onFocus: () => void
}

/** Edits of one text that followed each other, as one line: what the
 * latest did, and how many they are; opened, they follow it. */
function Run(props: At & { run: readonly HistoryEntry[]; open: boolean; toggle: () => void }) {
  const { entry } = props.run[0]
  return (
    // A row of the list, whose keys (j/k, Enter) are its own: the click is the
    // pointer's way to them.
    // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-noninteractive-element-interactions
    <li
      ref={props.itemRef}
      class={[styles.item, props.focused && styles.focused].filter(Boolean).join(' ')}
      aria-current={props.focused || undefined}
      onClick={props.onFocus}
    >
      <time class={styles.time} dateTime={new Date(entry.ts).toISOString()}>
        {dateTime(entry.ts)}
      </time>
      <span class={styles.description}>
        {entry.description} <span class={styles.state}>{props.run.length} edits</span>
      </span>
      <button type="button" aria-expanded={props.open} onMouseDown={keepFocus} onClick={props.toggle}>
        {props.open ? 'Hide' : 'Show'}
      </button>
    </li>
  )
}

function Item(props: At & { history: HistoryEntry; inRun: boolean }) {
  const { ops } = useServices()
  const { entry, undone_by } = props.history
  const undone = undone_by !== null
  return (
    // A row of the list, whose keys (j/k, Enter) are its own: the click is the
    // pointer's way to them.
    // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-noninteractive-element-interactions
    <li
      ref={props.itemRef}
      class={[
        styles.item,
        props.focused && styles.focused,
        undone && styles.undone,
        entry.shown && styles.shown,
        props.inRun && styles.inRun,
      ]
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
