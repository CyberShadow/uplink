import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Conversation, Entry, EntryId, Label, MessageContent, Sender } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus, lines, useKeymap } from '../keymap'
import { conversationKey, hasEntry, inScope, threadOf, type MailStore, type Thread, type View } from '../stores/mail'
import { respond, type Response } from '../compose/reply'
import { allMailPath, inboxPath, mailPath, searchPath } from './App'
import { Found } from './Found'
import { HtmlBody, QuoteToggle } from './HtmlBody'
import { MenuButton } from './Menu'
import { forwards, splitText } from './quote'
import { LabelPicker, MovePicker } from './LabelPicker'
import { composePath } from './Outgoing'
import { messagePath } from './Todos'
import styles from './Mail.module.css'
import { fileSize } from './size'
import { brief, dateTime } from '../format'
import { Icon, type IconName } from './Icon'

// Route params; preact-iso's own `query` prop is the URL's query string.
interface Props {
  /** Every account's Inbox, rather than a label's or a search's list. */
  inbox?: boolean
  account?: string
  label?: string
  terms?: string
  entry?: string
  message?: string
}

/** A label's or a search's list, or one of its conversations; or a
 * message's conversation, as a to-do links to it. */
export function Mail(props: Props) {
  const { mail, todos } = useServices()
  const { route } = useLocation()
  const [picking, setPicking] = useState<'label' | 'move' | null>(null)

  const { inbox, account, label, terms, message } = props
  const view: View = useMemo(
    () =>
      inbox
        ? { kind: 'inbox' }
        : message !== undefined
          ? { kind: 'message', message: Number(message) }
          : terms !== undefined
            ? { kind: 'search', query: terms }
            : label === undefined
              ? { kind: 'all', account: account! }
              : { kind: 'label', account: account!, label },
    [inbox, account, label, terms, message],
  )
  useEffect(() => mail.show(view), [mail, view])

  const pathFor = (entry?: number) => {
    switch (view.kind) {
      case 'label':
        return mailPath(view.account, view.label, entry)
      case 'all':
        return allMailPath(view.account, entry)
      case 'inbox':
        return inboxPath(entry)
      case 'search':
        return searchPath(view.query, entry)
      case 'message':
        return messagePath(view.message)
    }
  }

  const list = mail.list.value
  const conversations = list?.conversations ?? []
  // A message's conversation is open as soon as one of its entries is in.
  const openId =
    view.kind === 'message'
      ? (conversations[0]?.latest.id ?? null)
      : props.entry === undefined
        ? null
        : Number(props.entry)
  const opened = openId === null ? undefined : conversations.find((c) => hasEntry(c, openId))
  useEffect(() => mail.showConversation(opened ? openId : null), [mail, opened, openId])
  // Of the conversation open, the entries in the list's scope.
  const scope = list?.scope
  const shown = (scope && mail.open.value?.entries?.filter((e) => inScope(scope, e.labels))) ?? null
  const openThread: Thread | null =
    opened && shown && shown.length > 0
      ? { account: opened.latest.account, subject: shown.at(-1)!.subject, entries: shown }
      : null
  const targets: Thread[] =
    openId === null ? mail.targets.value.map(threadOf) : openThread ? [openThread] : []

  useEffect(() => {
    if (opened) mail.focusOn(opened)
  }, [mail, opened])

  const open = (conversation: Conversation) => {
    mail.focusOn(conversation)
    route(pathFor(conversation.latest.id))
  }
  /** The conversation `by` from the open one, if listed. */
  const sibling = (by: number): Conversation | undefined =>
    opened && conversations[conversations.indexOf(opened) + by]
  /** In a conversation, j and k open the next or previous one. */
  const openSibling = (by: number) => {
    const to = sibling(by)
    if (to) open(to)
  }
  /** In a conversation, [ and ] archive it and open the newer or older one. */
  const archiveAndOpen = (by: number) => {
    if (!openThread) return
    const to = sibling(by)
    mail.archive([openThread])
    if (to) open(to)
    else backToList()
  }
  const ifTargets = (action: (targets: Thread[]) => void) => () => {
    if (targets.length > 0) action(targets)
  }
  // A message by itself has no list: back is where it was opened from.
  const backToList = view.kind === 'message' ? () => history.back() : () => route(pathFor())

  const common: Actions = {
    archive: ifTargets((t) => {
      mail.archive(t)
      if (openId !== null) backToList()
    }),
    markRead: ifTargets((t) => mail.markRead(t, true)),
    markUnread: ifTargets((t) => {
      mail.markRead(t, false)
      if (openId !== null) backToList()
    }),
    delete: ifTargets((t) => {
      mail.trash(t)
      if (openId !== null) backToList()
    }),
    spam: ifTargets((t) => {
      mail.toggleSpam(t)
      if (openId !== null) backToList()
    }),
    star: ifTargets((t) => mail.toggleStar(t)),
    important: ifTargets((t) => mail.markImportant(t, true)),
    notImportant: ifTargets((t) => mail.markImportant(t, false)),
    label: ifTargets(() => setPicking('label')),
    move: ifTargets(() => setPicking('move')),
    // Unflags a conversation's flagged messages, or else flags its latest.
    flag: ifTargets((t) => {
      for (const thread of t) {
        const flagged = thread.entries.filter((e) => todos.flagged.value.has(e.message))
        const messages = flagged.length > 0 ? flagged.map((e) => e.message) : [thread.entries.at(-1)!.message]
        for (const message of new Set(messages)) todos.toggleFlag(message, thread.subject ?? '(no subject)')
      }
    }),
  }
  useKeymap(
    openId === null
      ? {
          ...common,
          ...lines(
            () => mail.moveFocus(1),
            () => mail.moveFocus(-1),
          ),
          open: () => {
            const focused = conversations.find((c) => conversationKey(c) === mail.focus.value)
            if (focused) open(focused)
          },
          select: () => {
            if (mail.focus.value !== null) mail.toggleSelected(mail.focus.value)
          },
          selectAll: () => mail.selectAll(),
          selectNone: () => mail.selectNone(),
          selectRead: () => mail.selectHaving('UNREAD', false),
          selectUnread: () => mail.selectHaving('UNREAD', true),
          selectStarred: () => mail.selectHaving('STARRED', true),
          selectUnstarred: () => mail.selectHaving('STARRED', false),
          // Else, home (`Shell`).
          ...(mail.selected.value.size > 0 && { up: () => mail.selectNone() }),
        }
      : {
          ...common,
          next: () => openSibling(1),
          previous: () => openSibling(-1),
          archiveNewer: () => archiveAndOpen(-1),
          archiveOlder: () => archiveAndOpen(1),
          back: backToList,
          up: backToList,
        },
  )

  return (
    <>
      {openId === null ? (
        <>
          {list?.found && <Found found={list.found} />}
          {conversations.length > 0 && <ListToolbar mail={mail} actions={common} />}
          <MessageList mail={mail} open={open} />
        </>
      ) : openThread ? (
        <>
          <ConversationToolbar
            mail={mail}
            actions={common}
            thread={openThread}
            back={backToList}
            // A message by itself is of no list to go through.
            place={
              view.kind === 'message'
                ? null
                : {
                    index: conversations.indexOf(opened!),
                    count: list!.next === null ? conversations.length : null,
                    newer: sibling(-1) ? () => openSibling(-1) : null,
                    older: sibling(1) ? () => openSibling(1) : null,
                  }
            }
          />
          <ConversationView
            key={conversationKey(opened!)}
            mail={mail}
            thread={openThread}
            entries={shown!}
            back={backToList}
            // The path names an entry of it, which may be the one leaving.
            reopen={(entry) => route(pathFor(entry), true)}
          />
        </>
      ) : (
        <p class={styles.placeholder}>
          {list?.loaded && !opened ? 'This conversation is not in the list.' : 'Loading…'}
        </p>
      )}
      {picking === 'label' && <LabelPicker targets={targets} close={() => setPicking(null)} />}
      {picking === 'move' && (
        <MovePicker
          targets={targets}
          from={view.kind === 'label' ? view.label : null}
          close={() => setPicking(null)}
        />
      )}
    </>
  )
}

/** What the keys, and the toolbar, do to the targets. */
type Actions = Record<
  | 'archive'
  | 'markRead'
  | 'markUnread'
  | 'delete'
  | 'spam'
  | 'star'
  | 'important'
  | 'notImportant'
  | 'label'
  | 'move'
  | 'flag',
  () => void
>

/** An icon button of a toolbar; `null` disables it. */
function ToolButton(props: { label: string; icon: IconName; action: (() => void) | null }) {
  return (
    <button
      type="button"
      class={styles.tool}
      aria-label={props.label}
      title={props.label}
      disabled={props.action === null}
      onMouseDown={keepFocus}
      onClick={props.action ?? undefined}
    >
      <Icon name={props.icon} />
    </button>
  )
}

/** What a toolbar does to the threads, Gmail's groups of buttons. */
function ThreadActions(props: { mail: MailStore; actions: Actions; threads: Thread[] }) {
  const { mail, actions, threads } = props
  const { todos } = useServices()
  const unread = threads.some((t) => mail.having(t, 'UNREAD'))
  const flagged = threads.every((t) => t.entries.some((e) => todos.flagged.value.has(e.message)))
  const starred = threads.every((t) => mail.having(t, 'STARRED'))
  const important = threads.every((t) => mail.having(t, 'IMPORTANT'))
  return (
    <>
      <span class={styles.group}>
        <ToolButton label="Archive" icon="archive" action={actions.archive} />
        <ToolButton label={mail.spam(threads) ? 'Not spam' : 'Report spam'} icon="octagon-x" action={actions.spam} />
        <ToolButton label="Delete" icon="trash" action={actions.delete} />
      </span>
      <span class={styles.group}>
        {unread ? (
          <ToolButton label="Mark as read" icon="mail-open" action={actions.markRead} />
        ) : (
          <ToolButton label="Mark as unread" icon="mail" action={actions.markUnread} />
        )}
        <ToolButton label={flagged ? 'Unflag' : 'Flag for follow-up'} icon="flag" action={actions.flag} />
      </span>
      <span class={styles.group}>
        <ToolButton label="Move to" icon="folder" action={actions.move} />
        <ToolButton label="Label" icon="tag" action={actions.label} />
        <MenuButton
          label="More"
          icon="ellipsis-vertical"
          groups={[
            [
              { label: starred ? 'Remove star' : 'Add star', action: actions.star },
              important
                ? { label: 'Mark as not important', action: actions.notImportant }
                : { label: 'Mark as important', action: actions.important },
            ],
          ]}
        />
      </span>
    </>
  )
}

/** Gmail's bar above the list: a box selecting all or none, with a menu
 * of which to select, and, while some are, what to do to them. */
function ListToolbar(props: { mail: MailStore; actions: Actions }) {
  const { mail, actions } = props
  const conversations = mail.list.value!.conversations
  const chosen = conversations.filter((c) => mail.selected.value.has(conversationKey(c))).map(threadOf)
  return (
    <div class={styles.toolbar} role="toolbar" aria-label="Selection">
      <span class={styles.select}>
        <input
          type="checkbox"
          aria-label="Select all"
          checked={chosen.length === conversations.length}
          indeterminate={chosen.length > 0 && chosen.length < conversations.length}
          onMouseDown={keepFocus}
          onClick={() => (chosen.length > 0 ? mail.selectNone() : mail.selectAll())}
        />
        <MenuButton
          label="Select"
          icon="chevron-down"
          start
          groups={[
            [
              { label: 'All', action: () => mail.selectAll() },
              { label: 'None', action: () => mail.selectNone() },
              { label: 'Read', action: () => mail.selectHaving('UNREAD', false) },
              { label: 'Unread', action: () => mail.selectHaving('UNREAD', true) },
              { label: 'Starred', action: () => mail.selectHaving('STARRED', true) },
              { label: 'Unstarred', action: () => mail.selectHaving('STARRED', false) },
            ],
          ]}
        />
      </span>
      {chosen.length > 0 && (
        <>
          <ThreadActions mail={mail} actions={actions} threads={chosen} />
          <span class={styles.chosen}>{chosen.length} selected</span>
        </>
      )}
    </div>
  )
}

/** Where the open conversation is in its list: `count` is `null` while
 * more are to load; `newer` and `older` open its neighbours, if listed. */
interface Place {
  index: number
  count: number | null
  newer: (() => void) | null
  older: (() => void) | null
}

/** Gmail's bar above a conversation: back to the list, what to do to it,
 * and where it is in the list, with the way to its neighbours. */
function ConversationToolbar(props: {
  mail: MailStore
  actions: Actions
  thread: Thread
  back: () => void
  place: Place | null
}) {
  const { place } = props
  return (
    <div class={styles.toolbar} role="toolbar" aria-label="Conversation actions">
      <span class={styles.group}>
        <ToolButton label="Back to the list" icon="arrow-left" action={props.back} />
      </span>
      <ThreadActions mail={props.mail} actions={props.actions} threads={[props.thread]} />
      {place && (
        <span class={styles.place}>
          {place.index + 1} of {place.count ?? 'many'}
          <ToolButton label="Newer" icon="chevron-left" action={place.newer} />
          <ToolButton label="Older" icon="chevron-right" action={place.older} />
        </span>
      )}
    </div>
  )
}

function MessageList(props: { mail: MailStore; open: (conversation: Conversation) => void }) {
  const { mail } = props
  const { todos } = useServices()
  const list = mail.list.value
  const focused = useRef<HTMLLIElement>(null)
  useEffect(() => {
    focused.current?.scrollIntoView({ block: 'nearest' })
  }, [mail.focus.value])

  if (!list?.loaded) return <p class={styles.placeholder}>Loading…</p>
  if (list.conversations.length === 0) return <p class={styles.placeholder}>{list.found ? 'No mail.' : 'No messages.'}</p>
  // With several accounts, each row says whose it is.
  const accounts = mail.accounts.value ?? []
  const address = (account: string) =>
    accounts.length > 1 ? accounts.find((a) => a.id === account)!.address : null
  const own = (account: string) => accounts.find((a) => a.id === account)?.address ?? ''
  return (
    <ul
      class={styles.list}
      aria-label="Messages"
      onScroll={(event) => {
        const el = event.currentTarget
        if (el.scrollTop + el.clientHeight > el.scrollHeight - 200) mail.loadMore()
      }}
    >
      {list.conversations.map((conversation) => {
        const key = conversationKey(conversation)
        const thread = threadOf(conversation)
        const { latest } = conversation
        const isFocused = key === mail.focus.value
        const isFlagged = thread.entries.some((e) => todos.flagged.value.has(e.message))
        const isSelected = mail.selected.value.has(key)
        const count = thread.entries.length
        return (
          <li
            key={key}
            ref={isFocused ? focused : undefined}
            class={[
              styles.row,
              isFocused && styles.focused,
              isSelected && styles.selected,
              mail.having(thread, 'UNREAD') && styles.unread,
            ]
              .filter(Boolean)
              .join(' ')}
            aria-current={isFocused || undefined}
            aria-selected={isSelected}
            onClick={() => props.open(conversation)}
          >
            <input
              type="checkbox"
              aria-label="Select"
              checked={isSelected}
              onMouseDown={keepFocus}
              onClick={(event) => {
                event.stopPropagation()
                mail.toggleSelected(key)
              }}
            />
            <Star mail={mail} thread={thread} />
            <span class={styles.from}>
              {senders(conversation.senders, own(latest.account))}
              {count > 1 && <span class={styles.count}> {count}</span>}
            </span>
            <span class={styles.summary}>
              <span class={styles.tags}>
                {isFlagged && <Flag />}
                {address(latest.account) && <span class={styles.account}>{address(latest.account)}</span>}
                <LabelChips mail={mail} thread={thread} />
              </span>
              <span class={styles.subject}>{latest.subject ?? '(no subject)'}</span>
              <span class={styles.snippet}>{latest.snippet}</span>
            </span>
            <span class={styles.clip}>
              {conversation.has_attachments && (
                <span title="Has attachments">
                  <Icon name="paperclip" />
                </span>
              )}
            </span>
            <time class={styles.date} dateTime={new Date(latest.received_at).toISOString()}>
              {brief(latest.received_at)}
            </time>
          </li>
        )
      })}
    </ul>
  )
}

/** A conversation's senders, as Gmail's rows name them: the one by name;
 * several by first names, `me` for the account's own address, the first
 * and the last two if there are many. */
export function senders(list: Sender[], own: string): string {
  const name = (s: Sender, brief: boolean) => {
    if (s.addr && s.addr.toLowerCase() === own.toLowerCase()) return 'me'
    const full = s.name ?? s.addr ?? '(unknown sender)'
    return brief && s.name ? s.name.split(/\s+/)[0] : full
  }
  if (list.length === 1) return name(list[0], false)
  const names = list.map((s) => name(s, true))
  return (names.length > 3 ? [names[0], '…', ...names.slice(-2)] : names).join(', ')
}

/** The conversation open: its messages oldest first, the unread ones and
 * the latest expanded. */
function ConversationView(props: {
  mail: MailStore
  thread: Thread
  entries: Entry[]
  back: () => void
  reopen: (entry: EntryId) => void
}) {
  const { mail, thread, entries, back, reopen } = props
  const { drafts, todos } = useServices()
  const { route } = useLocation()
  const latest = entries.at(-1)!

  const responder = (kind: Response, entry: Entry) => async () => {
    const content = await mail.loadMessage(entry.message)
    if (!content) return
    const own = mail.accounts.value?.find((a) => a.id === entry.account)?.address ?? ''
    const draft = respond(kind, content, own)
    const id = await drafts.create(entry.account, draft.content, draft.inReplyTo)
    if (!id) return
    route(composePath(id))
    if (kind === 'forward') await drafts.attachForwarded(id, content)
  }
  useKeymap({
    reply: responder('reply', latest),
    replyAll: responder('replyAll', latest),
    forward: responder('forward', latest),
  })

  // A message's own actions, from its menu. Those that take it out of the
  // view leave the conversation if it was all the view had, and else
  // reopen it by a message that stays.
  const actions = (entry: Entry): MessageActions => {
    const alone: Thread = { account: entry.account, subject: entry.subject, entries: [entry] }
    const leaving = (action: () => void) => () => {
      action()
      const staying = entries.filter((e) => e !== entry)
      if (staying.length === 0) back()
      else reopen(staying.at(-1)!.id)
    }
    return {
      reply: responder('reply', entry),
      replyAll: responder('replyAll', entry),
      forward: responder('forward', entry),
      trash: leaving(() => mail.trash([alone])),
      // Opening reads it: unread, the conversation is left, as Gmail does.
      markUnreadFrom: () => {
        mail.markUnreadFrom(thread, entry.id)
        back()
      },
      spam: mail.having(alone, 'SPAM') ? null : leaving(() => mail.toggleSpam([alone])),
      showOriginal: () => window.open(rawPath(entry.id), '_blank', 'noopener'),
      download: () => {
        const link = document.createElement('a')
        link.href = rawPath(entry.id)
        link.download = `${entry.subject ?? 'message'}.eml`
        link.click()
      },
    }
  }

  // Opening a conversation reads it, and so does mail arriving in it. The
  // thread is made anew each render; whether it is unread is the change.
  const unread = mail.having(thread, 'UNREAD')
  const current = useRef(thread)
  current.current = thread
  useEffect(() => {
    if (unread) mail.markRead([current.current], true, true)
  }, [mail, unread])

  // Which are expanded, by entry; as opened, until toggled.
  const [expanded, setExpanded] = useState<ReadonlyMap<EntryId, boolean>>(new Map())
  const [initially] = useState(() => {
    const label = mail.systemLabel(thread.account, 'UNREAD').id
    return new Set(entries.filter((e) => e.labels.includes(label)).map((e) => e.id))
  })
  const isExpanded = (entry: Entry) =>
    expanded.get(entry.id) ?? (entry === latest || initially.has(entry.id))

  return (
    <article class={styles.message}>
      <h1 class={styles.messageSubject}>
        {latest.subject ?? '(no subject)'}{' '}
        {thread.entries.some((e) => todos.flagged.value.has(e.message)) && <Flag />}
        <LabelChips mail={mail} thread={thread} />
      </h1>
      <ol class={styles.conversation} aria-label="Conversation">
        {entries.map((entry) => (
          <li key={entry.id}>
            <ConversationMessage
              mail={mail}
              entry={entry}
              expanded={isExpanded(entry)}
              toggle={() => setExpanded(new Map(expanded).set(entry.id, !isExpanded(entry)))}
              actions={actions(entry)}
            />
          </li>
        ))}
      </ol>
      <div class={styles.responses}>
        <button type="button" onMouseDown={keepFocus} onClick={responder('replyAll', latest)}>
          <Icon name="reply-all" />
          Reply to all
        </button>
        <button type="button" onMouseDown={keepFocus} onClick={responder('reply', latest)}>
          <Icon name="reply" />
          Reply
        </button>
        <button type="button" onMouseDown={keepFocus} onClick={responder('forward', latest)}>
          <Icon name="forward" />
          Forward
        </button>
      </div>
    </article>
  )
}

/** What can be done to one message of a conversation; `spam` is `null`
 * if it is spam already. */
interface MessageActions {
  reply: () => void
  replyAll: () => void
  forward: () => void
  trash: () => void
  markUnreadFrom: () => void
  spam: (() => void) | null
  showOriginal: () => void
  download: () => void
}

/** The entry's message as its account has it, raw (SPEC §8.9). */
function rawPath(entry: EntryId): string {
  return `/api/entries/${entry}/raw`
}

/** One message of a conversation: collapsed, its sender, snippet and
 * date; expanded, all of it, with its actions. */
function ConversationMessage(props: {
  mail: MailStore
  entry: Entry
  expanded: boolean
  toggle: () => void
  actions: MessageActions
}) {
  const { mail, entry, expanded, actions } = props
  // `null` if it failed to load.
  const [content, setContent] = useState<MessageContent | null | undefined>(undefined)
  const message = entry.message
  useEffect(() => {
    if (!expanded) return
    let current = true
    mail.loadMessage(message).then((loaded) => current && setContent(loaded))
    return () => {
      current = false
    }
  }, [mail, message, expanded])
  const date = content?.date_sent ? content.date_sent * 1000 : entry.received_at
  return (
    <section class={expanded ? styles.expanded : undefined} aria-expanded={expanded}>
      <header class={styles.messageHeader} onClick={props.toggle}>
        <span class={styles.from}>{address(entry.from_name, entry.from_addr)}</span>
        {!expanded && <span class={styles.snippet}>{entry.snippet}</span>}
        <time class={styles.date}>{dateTime(date)}</time>
        <Star mail={mail} thread={{ account: entry.account, subject: entry.subject, entries: [entry] }} />
        {expanded && (
          // Its own clicks, not the header's, which folds the message.
          <span class={styles.messageTools} onClick={(event) => event.stopPropagation()}>
            <button type="button" aria-label="Reply" title="Reply" onMouseDown={keepFocus} onClick={actions.reply}>
              <Icon name="reply" />
            </button>
            <MenuButton
              label="More"
              icon="ellipsis-vertical"
              groups={[
                [
                  { label: 'Reply', icon: 'reply', action: actions.reply },
                  { label: 'Reply to all', icon: 'reply-all', action: actions.replyAll },
                  { label: 'Forward', icon: 'forward', action: actions.forward },
                ],
                [
                  { label: 'Delete this message', icon: 'trash', action: actions.trash },
                  { label: 'Mark unread from here', icon: 'mail', action: actions.markUnreadFrom },
                ],
                ...(actions.spam ? [[{ label: 'Report spam', icon: 'octagon-x' as const, action: actions.spam }]] : []),
                [
                  { label: 'Download message', icon: 'download', action: actions.download },
                  { label: 'Show original', icon: 'code', action: actions.showOriginal },
                ],
              ]}
            />
          </span>
        )}
      </header>
      {expanded &&
        (content === undefined ? (
          <p class={styles.placeholder}>Loading…</p>
        ) : content === null ? (
          <p class={styles.placeholder}>The message could not be loaded.</p>
        ) : (
          <>
            <dl class={styles.headers}>
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
            </dl>
            <Attachments message={content} />
            {content.body_html === null ? (
              <TextBody text={content.body_text} fold={!forwards(content.subject)} />
            ) : (
              <HtmlBody html={content.body_html} fold={!forwards(content.subject)} />
            )}
          </>
        ))}
    </section>
  )
}

/** A plain-text body; `fold`: as `HtmlBody`'s. */
function TextBody(props: { text: string; fold: boolean }) {
  const [shown, setShown] = useState(false)
  const [written, quote] = props.fold ? splitText(props.text) : [props.text, null]
  return (
    <>
      <pre class={styles.body}>{written}</pre>
      {quote !== null && <QuoteToggle shown={shown} toggle={() => setShown(!shown)} />}
      {quote !== null && shown && <pre class={styles.body}>{quote}</pre>}
    </>
  )
}

/** The files attached, to download; not the images the body shows. */
function Attachments(props: { message: MessageContent }) {
  const files = props.message.attachments.filter((a) => !a.inline)
  if (files.length === 0) return null
  return (
    <ul class={styles.attachments} aria-label="Attachments">
      {files.map((a) => (
        <li key={a.index}>
          <a href={`/api/messages/${props.message.id}/attachments/${a.index}`} download={a.name ?? ''}>
            {a.name ?? `Attachment ${a.index + 1}`}
          </a>{' '}
          <span class={styles.size}>{fileSize(a.size)}</span>
        </li>
      ))}
    </ul>
  )
}

/** The labels worth showing beside a conversation: the user's own, of
 * any of its entries. */
function LabelChips(props: { mail: MailStore; thread: Thread }) {
  const ids = new Set(props.thread.entries.flatMap((e) => e.labels))
  const labels = [...ids]
    .map((id) => props.mail.labels.value.get(id))
    .filter((l): l is Label => l !== undefined && !l.system)
  return (
    <>
      {labels.map((label) => (
        <span key={label.id} class={styles.chip}>
          {label.name}
        </span>
      ))}
    </>
  )
}

/** Whether the thread has a star, and a button to toggle it. */
function Star(props: { mail: MailStore; thread: Thread }) {
  const { mail, thread } = props
  const starred = mail.having(thread, 'STARRED')
  return (
    <button
      type="button"
      class={[styles.star, starred && styles.starred].filter(Boolean).join(' ')}
      aria-label="Star"
      aria-pressed={starred}
      title={starred ? 'Starred' : 'Not starred'}
      onMouseDown={keepFocus}
      onClick={(event) => {
        event.stopPropagation()
        mail.toggleStar([thread])
      }}
    >
      <Icon name="star" filled={starred} />
    </button>
  )
}

function Flag() {
  return (
    <a href="/todos" class={styles.flag} title="Flagged: a to-do follows it up" onClick={(e) => e.stopPropagation()}>
      ⚑
    </a>
  )
}

function address(name: string | null, addr: string | null): string {
  if (name && addr) return `${name} <${addr}>`
  return name ?? addr ?? '(unknown)'
}

