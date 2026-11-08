// Drafts and the outbox: mail being written, and mail about to go out.

import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Draft, OutboxItem } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Mail.module.css'

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
          <span />
          <span class={styles.from}>{draft.content.to || '(no recipients)'}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{draft.content.subject || '(no subject)'}</span>
          </span>
          <time class={styles.date} dateTime={new Date(draft.updated_at).toISOString()}>
            {new Date(draft.updated_at).toLocaleString()}
          </time>
        </li>
      ))}
    </ul>
  )
}

export function Outbox() {
  const { drafts } = useServices()
  const items = drafts.outbox.value
  // Re-render every second, for the countdowns.
  const [, tick] = useState(0)
  const counting = items?.some((i) => i.state === 'queued')
  useEffect(() => {
    if (!counting) return
    const timer = setInterval(() => tick((t) => t + 1), 1000)
    return () => clearInterval(timer)
  }, [counting])

  if (!items) return <p class={styles.placeholder}>Loading…</p>
  if (items.length === 0) return <p class={styles.placeholder}>The outbox is empty.</p>
  return (
    <ul class={styles.list} aria-label="Outbox">
      {items.map((item) => (
        <li key={item.id} class={styles.row}>
          <span />
          <span class={styles.from}>{item.draft.content.to}</span>
          <span class={styles.summary}>
            <span class={styles.subject}>{item.draft.content.subject || '(no subject)'}</span>
            <span class={styles.snippet}> — {state(item)}</span>
          </span>
          {item.state === 'queued' || item.state === 'failed' ? (
            <button type="button" onMouseDown={keepFocus} onClick={() => drafts.cancelSend(item.id)}>
              {item.state === 'queued' ? 'Cancel' : 'Back to drafts'}
            </button>
          ) : (
            <span />
          )}
        </li>
      ))}
    </ul>
  )
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
