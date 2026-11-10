// Drafts and the outbox: mail being written, and what is not yet at the
// providers (SPEC §6.6): changes being pushed, and mail being sent.

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { AccountOutbox, Draft, MailProvider, OutboxItem, PendingChanges } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Mail.module.css'
import outgoing from './Outgoing.module.css'
import { dateTime } from '../format'

export function composePath(draft: string): string {
  return `/compose/${encodeURIComponent(draft)}`
}

export function Drafts() {
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
    next: () => setFocus((f) => Math.min(f + 1, list.length - 1)),
    previous: () => setFocus((f) => Math.max(f - 1, 0)),
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
  const { drafts, mail } = useServices()
  const accounts = drafts.outbox.value
  // Re-render every second, for the countdowns.
  const [, tick] = useState(0)
  const counting = accounts?.some((a) => a.sends.some((i) => i.state === 'queued'))
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
  const { drafts, ops } = useServices()
  const { account, failing, changes, sends } = props.account
  const name = providerName(props.provider)
  return (
    <section aria-label={account}>
      <h2 class={outgoing.account}>{account}</h2>
      {failing && (
        <p class={outgoing.failing} role="alert">
          Not reaching {name} since {dateTime(failing.since)}: {failing.error}
        </p>
      )}
      <ul class={styles.list} aria-label={`Outbox of ${account}`}>
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
              {(item.state === 'queued' || item.state === 'failed') && (
                <button type="button" onMouseDown={keepFocus} onClick={() => drafts.cancelSend(item.id)}>
                  {item.state === 'queued' ? 'Cancel' : 'Back to drafts'}
                </button>
              )}
            </span>
          </li>
        ))}
        {changes.map((change) => (
          <li key={`op:${change.op?.id ?? 'earlier'}`} class={styles.row}>
            <Unused />
            <span class={styles.from}>{change.op ? dateTime(change.op.ts) : 'Earlier'}</span>
            <span class={styles.summary} title={changeState(change, failing !== null, name)}>
              <span class={styles.subject}>{change.op?.description ?? 'Changes made earlier'}</span>
              <span class={styles.snippet}>{changeState(change, failing !== null, name)}</span>
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
      </ul>
    </section>
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
      return 'sending…'
    case 'sent':
      return 'sent'
    case 'failed':
      return `not sent: ${item.error}`
  }
}
