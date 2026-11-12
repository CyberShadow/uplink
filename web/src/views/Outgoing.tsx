// Drafts and the outbox: mail being written, and what is not yet at the
// providers (SPEC §6.6): changes being pushed, and mail and Matrix
// messages being sent.

import { tagName } from '../roomTags'
import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type {
  AccountOutbox,
  Draft,
  MailProvider,
  OutboxItem,
  Pending,
  PendingChanges,
} from '../generated/protocol'
import { useServices } from '../context'
import { useTitle } from '../title'
import { keepFocus, lines, useKeymap } from '../keymap'
import styles from './Mail.module.css'
import outgoing from './Outgoing.module.css'
import { dateTime } from '../format'
import { MatrixSendActions, matrixSendState } from './Room'
import { sendSummary } from '../matrix'

export function composePath(draft: string): string {
  return `/compose/${encodeURIComponent(draft)}`
}

export function Drafts() {
  useTitle('Drafts')
  const { drafts } = useServices()
  const { route } = useLocation()
  const loaded = drafts.drafts.value
  const list = loaded ? [...loaded.values()].sort((a, b) => b.updated_at - a.updated_at) : []
  const [focus, setFocus] = useState(0)
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [focus])

  const open = (draft: Draft | undefined) => draft && route(composePath(draft.id))
  useKeymap({
    ...lines(
      () => setFocus((f) => Math.min(f + 1, list.length - 1)),
      () => setFocus((f) => Math.max(f - 1, 0)),
    ),
    open: () => open(list[focus]),
  })

  if (!loaded) return <p class={styles.placeholder}>Loading…</p>
  if (list.length === 0) return <p class={styles.placeholder}>No drafts.</p>
  return (
    <ul class={styles.list} aria-label="Drafts">
      {list.map((draft, i) => (
        <li
          key={draft.id}
          ref={i === focus ? focused : undefined}
          class={[styles.row, i === focus && styles.focused].filter(Boolean).join(' ')}
          aria-current={i === focus || undefined}
          onClick={() => open(draft)}
        >
          <Unused />
          <span class={styles.from}>{draft.content.to || '(no recipients)'}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{draft.content.subject || '(no subject)'}</span>
          </span>
          <span />
          <time class={styles.date} dateTime={new Date(draft.updated_at).toISOString()}>
            {dateTime(draft.updated_at)}
          </time>
        </li>
      ))}
    </ul>
  )
}

export function Outbox() {
  useTitle('Outbox')
  const { drafts, mail } = useServices()
  const accounts = drafts.outbox.value
  // Re-render every second, for the countdowns.
  const [, tick] = useState(0)
  const counting = accounts?.some(
    ({ pending }) => pending.kind === 'mail' && pending.sends.some((i) => i.state === 'queued'),
  )
  useEffect(() => {
    if (!counting) return
    const timer = setInterval(() => tick((t) => t + 1), 1000)
    return () => clearInterval(timer)
  }, [counting])

  if (!accounts) return <p class={styles.placeholder}>Loading…</p>
  if (accounts.length === 0) {
    return <p class={styles.placeholder}>The outbox is empty: everything you did is at the providers.</p>
  }
  const provider = (account: string) => mail.accounts.value?.find((a) => a.id === account)?.provider
  return (
    <div class={outgoing.outbox}>
      {accounts.map((account) => (
        <AccountSection key={account.account} account={account} provider={provider(account.account)} />
      ))}
    </div>
  )
}

function AccountSection(props: { account: AccountOutbox; provider: MailProvider | undefined }) {
  const { account, failing, pending } = props.account
  const name = pending.kind === 'matrix' ? 'the homeserver' : providerName(props.provider)
  return (
    <section aria-label={account}>
      <h2 class={outgoing.account}>{account}</h2>
      {failing && (
        <p class={outgoing.failing} role="alert">
          Not reaching {name} since {dateTime(failing.since)}: {failing.error}
        </p>
      )}
      <ul class={styles.list} aria-label={`Outbox of ${account}`}>
        {pending.kind === 'mail' ? (
          <MailRows account={account} pending={pending} failing={failing !== null} provider={name} />
        ) : (
          <MatrixRows pending={pending} failing={failing !== null} />
        )}
      </ul>
    </section>
  )
}

function MatrixRows(props: { pending: Extract<Pending, { kind: 'matrix' }>; failing: boolean }) {
  const { matrix, ops } = useServices()
  const { sends, memberships, space_orders, room_tags, receipts } = props.pending
  const room = (id: number) => matrix.rooms.value?.find((r) => r.id === id)?.name ?? 'a room'
  return (
    <>
      {space_orders.map(({ op, spaces, refused }) => (
        <li key={`space-order:${op.id}`} class={styles.row}>
          <Unused />
          <span class={styles.from}>{dateTime(op.ts)}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{op.description}</span>
            <span class={styles.snippet}>
              {refused !== null
                ? `the homeserver refused: ${refused}`
                : `${props.failing ? 'waiting' : 'to go'}: ${spaces.map(room).join(', ')}`}
            </span>
          </span>
          <span />
          <span class={outgoing.actions}>
            <button type="button" onMouseDown={keepFocus} onClick={() => ops.undo(op.id)}>
              Undo
            </button>
          </span>
        </li>
      ))}
      {room_tags.map(({ op, changes, refused }) => (
        <li key={`room-tags:${op.id}`} class={styles.row}>
          <Unused />
          <span class={styles.from}>{dateTime(op.ts)}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{op.description}</span>
            <span class={styles.snippet}>
              {refused !== null
                ? `the homeserver refused: ${refused}`
                : `${props.failing ? 'waiting' : 'to go'}: ${changes
                    .map((c) => `${c.desired ? '+' : '−'}${tagName(c.tag)} ${room(c.room)}`)
                    .join(', ')}`}
            </span>
          </span>
          <span />
          <span class={outgoing.actions}>
            <button type="button" onMouseDown={keepFocus} onClick={() => ops.undo(op.id)}>
              Undo
            </button>
          </span>
        </li>
      ))}
      {memberships.map(({ room: id, op, refused }) => (
        <li key={`membership:${id}`} class={styles.row}>
          <Unused />
          <span class={styles.from}>{dateTime(op.ts)}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{op.description}</span>
            <span class={styles.snippet}>
              {refused !== null ? `the homeserver refused: ${refused}` : props.failing ? 'waiting' : 'to go'}
            </span>
          </span>
          <span />
          <span class={outgoing.actions}>
            <button type="button" onMouseDown={keepFocus} onClick={() => ops.undo(op.id)}>
              Undo
            </button>
          </span>
        </li>
      ))}
      {receipts.length > 0 && (
        <li key="receipts" class={styles.row}>
          <Unused />
          <span class={styles.from}>Read receipts</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{receipts.map(room).join(', ')}</span>
            <span class={styles.snippet}>{props.failing ? 'waiting' : 'to go'}</span>
          </span>
          <span />
          <span />
        </li>
      )}
      {sends.map((item) => (
        <li key={`matrix:${item.id}`} class={styles.row}>
          <Unused />
          <span class={styles.from}>{room(item.room)}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{sendSummary(item.content)}</span>
            <span class={styles.snippet}>{matrixSendState(item)}</span>
          </span>
          <span />
          <span class={outgoing.actions}>
            <MatrixSendActions item={item} />
          </span>
        </li>
      ))}
    </>
  )
}

function MailRows(props: {
  account: string
  pending: Extract<Pending, { kind: 'mail' }>
  failing: boolean
  provider: string
}) {
  const { drafts, ops } = useServices()
  const { account, failing, provider: name } = props
  const { changes, sends } = props.pending
  return (
    <>
        {sends.map((item) => (
          <li key={`send:${item.id}`} class={styles.row}>
            <Unused />
            <span class={styles.from}>{item.draft.content.to}</span>
            <span class={styles.summary}>
              <span class={styles.subject}>{item.draft.content.subject || '(no subject)'}</span>
              <span class={styles.snippet}>{state(item)}</span>
            </span>
            <span />
            <span class={outgoing.actions}>
              {item.wanted && item.state !== 'sent' && (
                <button type="button" onMouseDown={keepFocus} onClick={() => drafts.cancelSend(item.id)}>
                  {item.state === 'refused' ? 'Back to drafts' : 'Cancel'}
                </button>
              )}
              {!item.wanted && item.state === 'sent' && (
                <button type="button" onMouseDown={keepFocus} onClick={() => drafts.keepSent(item.id)}>
                  OK
                </button>
              )}
            </span>
          </li>
        ))}
        {changes.map((change) => (
          <li key={`op:${change.op?.id ?? 'earlier'}`} class={styles.row}>
            <Unused />
            <span class={styles.from}>{change.op ? dateTime(change.op.ts) : 'Earlier'}</span>
            <span class={styles.summary} title={changeState(change, failing, name)}>
              <span class={styles.subject}>{change.op?.description ?? 'Changes made earlier'}</span>
              <span class={styles.snippet}>{changeState(change, failing, name)}</span>
            </span>
            <span />
            <span class={outgoing.actions}>
              {change.refused.length > 0 && (
                <button
                  type="button"
                  onMouseDown={keepFocus}
                  onClick={() => drafts.keepObserved(account, change.op?.id ?? null)}
                >
                  Keep {name}’s
                </button>
              )}
              {change.op && (
                <button type="button" onMouseDown={keepFocus} onClick={() => ops.undo(change.op!.id)}>
                  Undo
                </button>
              )}
            </span>
          </li>
        ))}
    </>
  )
}

/** The cells of a message row's check box and star, which these rows have
 * no use for: the list's columns are a message list's (Mail.module.css). */
function Unused() {
  return (
    <>
      <span />
      <span />
    </>
  )
}

function providerName(provider: MailProvider | undefined): string {
  switch (provider) {
    case 'gmail':
      return 'Gmail'
    case 'm365':
      return 'Microsoft 365'
    case 'imap':
    case 'pop3':
      return 'the mail server'
    case undefined:
      return 'the provider'
  }
}

function changeState(change: PendingChanges, failing: boolean, provider: string): string {
  const messages = (n: number) => (n === 1 ? '1 message' : `${n} messages`)
  const refused = change.refused.map((r) => `${provider} refused ${messages(r.entries)}: ${r.reason}`)
  const waiting = change.entries - change.refused.reduce((n, r) => n + r.entries, 0)
  const parts = waiting > 0 ? [`${messages(waiting)} ${failing ? 'waiting' : 'to go'}`] : []
  return [...parts, ...refused].join('; ')
}

function state(item: OutboxItem): string {
  switch (item.state) {
    case 'queued': {
      const seconds = Math.max(0, Math.ceil((item.not_before - Date.now()) / 1000))
      return `sending in ${seconds} s`
    }
    case 'sending':
      return item.wanted ? 'sending…' : 'sending; cancelling if it does not go out'
    case 'unknown':
      return `may have been sent (${item.error}); checking${item.wanted ? '' : ', cancelling if it was not'}`
    case 'sent':
      return item.wanted ? 'sent; waiting for its copy' : 'cannot undo (already sent)'
    case 'refused':
      return `not sent: ${item.error}`
  }
}
