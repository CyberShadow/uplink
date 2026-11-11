// A search of a room's messages (SPEC §12.11), from the box at the top,
// in the panel beside its timeline, as Element's: the messages that have
// every word, newest first, their words marked; a click shows one in the
// timeline, or in its thread.

import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import type { Room, RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import { dateTime } from '../format'
import { displayName, senderColor, shown } from '../matrix'
import styles from './Room.module.css'
// Its header as the room info's.
import panel from './RoomInfo.module.css'

/** Keyed on `words`. */
export function RoomSearch(props: { room: Room; words: string; close: () => void; open: (event: RoomEvent) => void }) {
  const { room, words } = props
  const { matrix } = useServices()
  // The search shown: its words, what was found, and whether there is more.
  const [found, setFound] = useState<{ words: string; events: RoomEvent[]; more: boolean } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [searching, setSearching] = useState(false)
  // The latest search, so that an earlier one answered late is dropped.
  const latest = useRef(0)

  const search = useCallback(
    async (words: string, after: { events: RoomEvent[] } | null) => {
      const id = ++latest.current
      setSearching(true)
      setError(null)
      try {
        const response = await matrix.search(room.id, words, after?.events.at(-1)?.id ?? null)
        if (id !== latest.current) return
        setFound({ words, events: [...(after?.events ?? []), ...response.events], more: response.more })
      } catch (e) {
        if (id === latest.current) setError((e as Error).message)
      } finally {
        if (id === latest.current) setSearching(false)
      }
    },
    [matrix, room.id],
  )
  useEffect(() => void search(words, null), [search, words])

  return (
    <aside class={styles.thread} aria-label="Search">
      <header class={panel.header}>
        <h2>Search messages</h2>
        <button type="button" onMouseDown={keepFocus} onClick={props.close} aria-label="Close search">
          ×
        </button>
      </header>
      {error !== null && (
        <p class={styles.error} role="alert">
          {error}
        </p>
      )}
      {found !== null && (
        <ol class={styles.results} aria-label="Results">
          {found.events.length === 0 && <li class={`${styles.muted} ${styles.searchLine}`}>No messages found.</li>}
          {found.events.map((event) => (
            <li key={event.id}>
              <button type="button" class={styles.result} onMouseDown={keepFocus} onClick={() => props.open(event)}>
                <span>
                  <span style={{ color: senderColor(event.sender) }}>
                    {displayName(event.sender, event.sender_name)}
                  </span>{' '}
                  <span class={styles.muted}>{dateTime(event.ts)}</span>
                </span>
                <span>
                  <Marked text={shown(event).body ?? ''} words={found.words} />
                </span>
              </button>
            </li>
          ))}
          {found.more && (
            <li class={styles.searchLine}>
              <button
                type="button"
                disabled={searching}
                onMouseDown={keepFocus}
                onClick={() => search(found.words, found)}
              >
                More
              </button>
            </li>
          )}
        </ol>
      )}
    </aside>
  )
}

/** The text, the words in it marked, ignoring case. */
export function Marked(props: { text: string; words: string }) {
  const words = props.words.split(/\s+/).filter((w) => w !== '')
  const pattern = new RegExp(`(${words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})`, 'gi')
  return <>{props.text.split(pattern).map((part, i) => (i % 2 === 1 ? <mark key={i}>{part}</mark> : part))}</>
}
