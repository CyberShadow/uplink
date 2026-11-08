import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Entry, Label, MessageContent } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import type { MailStore, View } from '../stores/mail'
import { respond, type Response } from '../compose/reply'
import { mailPath, searchPath } from './App'
import { HtmlBody } from './HtmlBody'
import { LabelPicker } from './LabelPicker'
import { labelName } from './Nav'
import { composePath } from './Outgoing'
import { messagePath } from './Todos'
import styles from './Mail.module.css'

// Route params; preact-iso's own `query` prop is the URL's query string.
interface Props {
  account?: string
  label?: string
  terms?: string
  entry?: string
  message?: string
}

/** A label's or a search's list, or one of its messages; or a message by
 * itself, as a to-do links to it. */
export function Mail(props: Props) {
  const { mail, todos } = useServices()
  const { route } = useLocation()
  const [picking, setPicking] = useState(false)

  const { account, label, terms, message } = props
  const view: View = useMemo(
    () =>
      message !== undefined
        ? { kind: 'message', message: Number(message) }
        : terms !== undefined
          ? { kind: 'search', query: terms }
          : { kind: 'label', account: account!, label: label! },
    [account, label, terms, message],
  )
  useEffect(() => mail.show(view), [mail, view])

  const pathFor = (entry?: number) => {
    switch (view.kind) {
      case 'label':
        return mailPath(view.account, view.label, entry)
      case 'search':
        return searchPath(view.query, entry)
      case 'message':
        return messagePath(view.message)
    }
  }

  const list = mail.list.value
  const entries = list?.entries ?? []
  // A message by itself is open as soon as one of its entries is in.
  const openId =
    view.kind === 'message' ? (entries[0]?.id ?? null) : props.entry === undefined ? null : Number(props.entry)
  const opened = openId === null ? undefined : entries.find((e) => e.id === openId)
  const targets = openId === null ? mail.targets.value : opened ? [opened] : []

  useEffect(() => {
    if (openId !== null) mail.focus.value = openId
  }, [mail, openId])

  const open = (entry: Entry) => {
    mail.focus.value = entry.id
    route(pathFor(entry.id))
  }
  /** In a message, j and k open the next or previous one. */
  const openSibling = (by: number) => {
    const index = entries.findIndex((e) => e.id === openId)
    const sibling = entries[index + by]
    if (sibling) open(sibling)
  }
  const ifTargets = (action: (targets: Entry[]) => void) => () => {
    if (targets.length > 0) action(targets)
  }
  // A message by itself has no list: back is where it was opened from.
  const backToList = view.kind === 'message' ? () => history.back() : () => route(pathFor())

  const common = {
    archive: ifTargets((t) => {
      mail.archive(t)
      if (openId !== null) backToList()
    }),
    markRead: ifTargets((t) => mail.markRead(t, true)),
    markUnread: ifTargets((t) => {
      mail.markRead(t, false)
      if (openId !== null) backToList()
    }),
    star: ifTargets((t) => mail.toggleStar(t)),
    label: ifTargets(() => setPicking(true)),
    flag: ifTargets((t) => {
      for (const entry of new Map(t.map((e) => [e.message, e])).values()) {
        todos.toggleFlag(entry.message, entry.subject ?? '(no subject)')
      }
    }),
  }
  useKeymap(
    openId === null
      ? {
          ...common,
          next: () => mail.moveFocus(1),
          previous: () => mail.moveFocus(-1),
          open: () => {
            const focused = entries.find((e) => e.id === mail.focus.value)
            if (focused) open(focused)
          },
          select: () => {
            if (mail.focus.value !== null) mail.toggleSelected(mail.focus.value)
          },
          selectAll: () => mail.selectAll(),
          selectNone: () => mail.selectNone(),
        }
      : {
          ...common,
          next: () => openSibling(1),
          previous: () => openSibling(-1),
          back: backToList,
        },
  )

  return (
    <>
      {openId === null ? (
        <MessageList mail={mail} open={open} />
      ) : opened ? (
        <MessageView mail={mail} entry={opened} />
      ) : (
        <p class={styles.placeholder}>{list?.loaded ? 'This message is not in the list.' : 'Loading…'}</p>
      )}
      {picking && <LabelPicker targets={targets} close={() => setPicking(false)} />}
    </>
  )
}

function MessageList(props: { mail: MailStore; open: (entry: Entry) => void }) {
  const { mail } = props
  const { todos } = useServices()
  const list = mail.list.value
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [mail.focus.value])

  if (!list?.loaded) return <p class={styles.placeholder}>Loading…</p>
  if (list.entries.length === 0) return <p class={styles.placeholder}>No messages.</p>
  return (
    <ul
      class={styles.list}
      aria-label="Messages"
      onScroll={(event) => {
        const el = event.currentTarget
        if (el.scrollTop + el.clientHeight > el.scrollHeight - 200) mail.loadMore()
      }}
    >
      {list.entries.map((entry) => {
        const isFocused = entry.id === mail.focus.value
        const isFlagged = todos.flagged.value.has(entry.message)
        const isSelected = mail.selected.value.has(entry.id)
        return (
          <li
            key={entry.id}
            ref={isFocused ? focused : undefined}
            class={[
              styles.row,
              isFocused && styles.focused,
              isSelected && styles.selected,
              isUnread(mail, entry) && styles.unread,
            ]
              .filter(Boolean)
              .join(' ')}
            aria-current={isFocused || undefined}
            aria-selected={isSelected}
            onClick={() => props.open(entry)}
          >
            <input
              type="checkbox"
              aria-label="Select"
              checked={isSelected}
              onMouseDown={keepFocus}
              onClick={(event) => {
                event.stopPropagation()
                mail.toggleSelected(entry.id)
              }}
            />
            <span class={styles.from}>{sender(entry)}</span>
            <span class={styles.summary}>
              {isFlagged && <Flag />}
              <LabelChips mail={mail} entry={entry} />
              <span class={styles.subject}>{entry.subject ?? '(no subject)'}</span>
              <span class={styles.snippet}> — {entry.snippet}</span>
            </span>
            <time class={styles.date} dateTime={new Date(entry.received_at).toISOString()}>
              {shortDate(entry.received_at)}
            </time>
          </li>
        )
      })}
    </ul>
  )
}

function MessageView(props: { mail: MailStore; entry: Entry }) {
  const { mail, entry } = props
  const { drafts, todos } = useServices()
  const { route } = useLocation()
  // `null` if it failed to load.
  const [content, setContent] = useState<MessageContent | null | undefined>(undefined)

  const message = entry.message
  useEffect(() => {
    let current = true
    setContent(undefined)
    mail.loadMessage(message).then((loaded) => current && setContent(loaded))
    return () => {
      current = false
    }
  }, [mail, message])

  const responder = (kind: Response) => async () => {
    if (!content) return
    const own = mail.accounts.value?.find((a) => a.id === entry.account)?.address ?? ''
    const draft = respond(kind, content, own)
    const id = await drafts.create(entry.account, draft.content, draft.inReplyTo)
    if (id) route(composePath(id))
  }
  useKeymap({ reply: responder('reply'), replyAll: responder('replyAll'), forward: responder('forward') })

  // Opening a message reads it.
  const unread = isUnread(mail, entry)
  useEffect(() => {
    if (unread) mail.markRead([entry], true, true)
  }, [mail, entry, unread])

  return (
    <article class={styles.message}>
      <h1 class={styles.messageSubject}>
        {entry.subject ?? '(no subject)'} {todos.flagged.value.has(entry.message) && <Flag />}
        <LabelChips mail={mail} entry={entry} />
      </h1>
      {content === undefined ? (
        <p class={styles.placeholder}>Loading…</p>
      ) : content === null ? (
        <p class={styles.placeholder}>The message could not be loaded.</p>
      ) : (
        <>
          <dl class={styles.headers}>
            <dt>From</dt>
            <dd>{address(content.from_name, content.from_addr)}</dd>
            {content.to_addrs.length > 0 && (
              <>
                <dt>To</dt>
                <dd>{content.to_addrs.join(', ')}</dd>
              </>
            )}
            {content.cc_addrs.length > 0 && (
              <>
                <dt>Cc</dt>
                <dd>{content.cc_addrs.join(', ')}</dd>
              </>
            )}
            <dt>Date</dt>
            <dd>{new Date(content.date_sent === null ? entry.received_at : content.date_sent * 1000).toLocaleString()}</dd>
          </dl>
          {content.body_html === null ? (
            <pre class={styles.body}>{content.body_text}</pre>
          ) : (
            <HtmlBody html={content.body_html} />
          )}
        </>
      )}
    </article>
  )
}

/** The entry's labels worth showing beside it: user labels and Starred. */
function LabelChips(props: { mail: MailStore; entry: Entry }) {
  const labels = props.entry.labels
    .map((id) => props.mail.labels.value.get(id))
    .filter((l): l is Label => l !== undefined && (!l.system || l.external_id === 'STARRED'))
  return (
    <>
      {labels.map((label) =>
        label.external_id === 'STARRED' ? (
          <span key={label.id} class={styles.star} title="Starred">
            ★
          </span>
        ) : (
          <span key={label.id} class={styles.chip}>
            {labelName(label)}
          </span>
        ),
      )}
    </>
  )
}

function Flag() {
  return (
    <a href="/todos" class={styles.flag} title="Flagged: a to-do follows it up" onClick={(e) => e.stopPropagation()}>
      ⚑
    </a>
  )
}

function isUnread(mail: MailStore, entry: Entry): boolean {
  return entry.labels.some((id) => mail.labels.value.get(id)?.external_id === 'UNREAD')
}

function sender(entry: Entry): string {
  return entry.from_name ?? entry.from_addr ?? '(unknown sender)'
}

function address(name: string | null, addr: string | null): string {
  if (name && addr) return `${name} <${addr}>`
  return name ?? addr ?? '(unknown)'
}

function shortDate(ms: number): string {
  const date = new Date(ms)
  const now = new Date()
  if (date.toDateString() === now.toDateString()) {
    return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
  }
  if (date.getFullYear() === now.getFullYear()) {
    return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
  }
  return date.toLocaleDateString()
}
