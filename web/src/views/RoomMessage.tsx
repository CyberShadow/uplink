// A message's details beside its room's timeline (SPEC §12.11), opened by
// Enter on its line: who sent it and when, what it is, who has read up to
// it, its reactions and edits, and its source. While it is open, it shows
// the line that has the focus, as j/k move it.

import type { MemberReceipt, Room, RoomEvent } from '../generated/protocol'
import { dateTime } from '../format'
import { keepFocus } from '../keymap'
import { displayName } from '../matrix'
import { pretty, Versions } from './RoomEvent'
import panel from './RoomInfo.module.css'
import styles from './Room.module.css'

export function MessageDetails(props: {
  room: Room
  event: RoomEvent
  readers: readonly MemberReceipt[]
  close: () => void
}) {
  const { room, event } = props
  const kind = event.msgtype === null ? event.kind : `${event.kind} (${event.msgtype})`
  return (
    <aside class={panel.panel} aria-label="Message details">
      <header class={panel.header}>
        <h2>Message</h2>
        <button type="button" onMouseDown={keepFocus} onClick={props.close} aria-label="Close message details">
          ×
        </button>
      </header>
      <dl class={styles.facts}>
        <dt>From</dt>
        <dd>
          {displayName(event.sender, event.sender_name)} <span class={panel.muted}>{event.sender}</span>
        </dd>
        <dt>Sent</dt>
        <dd>{dateTime(event.ts)}</dd>
        <dt>Type</dt>
        <dd>{kind}</dd>
        <dt>Event</dt>
        <dd class={styles.id}>{event.event_id}</dd>
        <dt>Encryption</dt>
        <dd>{{ none: 'None', decrypted: 'Decrypted', undecryptable: 'Unable to decrypt' }[event.encryption]}</dd>
      </dl>
      {props.readers.length > 0 && (
        <section aria-label="Read up to here by">
          <h3>Read up to here by</h3>
          <p>{props.readers.map((r) => displayName(r.member.user, r.member.name)).join(', ')}</p>
        </section>
      )}
      {event.reactions.length > 0 && (
        <section aria-label="Reactions">
          <h3>Reactions</h3>
          <ul class={styles.history}>
            {event.reactions.map((r) => (
              <li key={r.key}>
                {r.key} {r.senders.map((s) => displayName(s.sender, s.sender_name)).join(', ')}
              </li>
            ))}
          </ul>
        </section>
      )}
      {event.edit !== null && (
        <section aria-label="Edits">
          <h3>Edits</h3>
          <Versions key={event.id} room={room} event={event} />
        </section>
      )}
      <section aria-label="Source">
        <h3>Source</h3>
        <pre class={styles.raw}>{pretty(event.raw)}</pre>
      </section>
    </aside>
  )
}
