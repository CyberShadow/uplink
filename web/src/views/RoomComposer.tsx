// The box to write to a room or one of its threads (SPEC §12.11):
// Markdown, and commands as Element's (`/me`, `/plain`, `/invite`…), with
// completions of commands, members and emoji; a reply to a message, or an
// edit of one's own (Up edits the last), which begins from the text the
// server makes of it; and, but in a thread, files, attached, pasted or
// dropped, sent with it. What is written, what it replies to and its
// files are the box's draft (§7.6): the server's, saved as typed, and so
// every tab's.

import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import type { Room, RoomBox, RoomEvent, RoomEventId } from '../generated/protocol'
import { parse } from '../commands'
import { diff, movePosition } from '../compose/splice'
import { useServices } from '../context'
import { byShortcode } from '../emoji'
import { type Actions, focusShown, keepFocus, useKeymap } from '../keymap'
import { displayName, senderColor, shown, text as message } from '../matrix'
import { completing, type Suggestion, Suggestions, useEmoji, useSuggestions } from './Autocomplete'
import { Icon } from './Icon'
import { useOpenJoined } from './RoomStarter'
import styles from './Room.module.css'

/** What the composer writes: a message, a reply to one, or an edit of
 * one of the account's own. */
export type Mode =
  | { type: 'new' }
  // `back`: begun by a key on its line, which Esc, cancelling it, goes
  // back to.
  | { type: 'reply'; event: RoomEvent; back: boolean }
  | { type: 'edit'; event: RoomEvent; back: boolean }

/** What the box at `at` writes, and how to change that. A reply is its
 * draft's (SPEC §7.6), so every tab's; an edit is this tab's alone, the
 * draft kept apart. Set to `new`, it leaves what it is in: an edit, or
 * else a reply. */
export function useMode(at: RoomBox): [Mode, (mode: Mode) => void] {
  const { matrix } = useServices()
  const [editing, setEditing] = useState<Extract<Mode, { type: 'edit' }> | null>(null)
  // Whether the reply was begun by a key on its line, here.
  const [back, setBack] = useState(false)
  const reply = matrix.drafts.draft(at)?.reply_to ?? null
  const mode: Mode = editing ?? (reply ? { type: 'reply', event: reply, back } : { type: 'new' })
  const setMode = (next: Mode) => {
    setEditing(next.type === 'edit' ? next : null)
    if (next.type === 'edit' || (editing && next.type === 'new')) return
    setBack(next.type === 'reply' && next.back)
    const event = next.type === 'reply' ? next.event : null
    if (event?.id !== reply?.id) void matrix.drafts.setReply(at, event)
  }
  return [mode, setMode]
}

/** How often a typing notice is renewed while typing: the homeserver
 * forgets it after 4 s, as the SDK sends it. */
const TYPING_RENEW = 3000

/** Of a box whose drafts are loaded. */
export function Composer(props: {
  room: Room
  /** The box it is: its draft is what it writes. */
  at: RoomBox
  textarea: preact.RefObject<HTMLTextAreaElement>
  mode: Mode
  setMode: (mode: Mode) => void
  /** The account's last message of text, which Up edits. */
  lastOwn: RoomEvent | null
  /** Of a thread's box, the thread's root (a Matrix event id). */
  thread: string | null
  /** Scrolls what the box writes to, by the page keys, from the box. */
  scrolling: Actions
  /** Gives the focus to an event's line, if shown. */
  focusLine: (event: RoomEventId) => void
  /** Where Esc goes from the box, as it was opened from there by a key:
   * else, nowhere. */
  back: (() => void) | null
}) {
  const { room, at, mode, setMode, thread } = props
  const { matrix } = useServices()
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const session = matrix.drafts.session(at)
  const draft = session.content.value.text
  // Files go with a room's messages, not a thread's.
  const attachable = at.thread === null
  const files = matrix.drafts.draft(at)!.attachments
  const attaching = matrix.drafts.attaching.value.filter((a) => attachable && a.room === room.id)
  const attach = (added: readonly File[]) => void matrix.drafts.attach(room.id, added)
  // Why the last message could not be sent, as a command uplink does not
  // know; until the box changes.
  const [error, setError] = useState<string | null>(null)
  const [caret, setCaret] = useState(0)
  // Completions dismissed, until the text changes.
  const [dismissed, setDismissed] = useState(false)
  const [selected, setSelected] = useState(0)
  // Where the caret goes once a completion is in the box.
  const placeCaret = useRef<number | null>(null)
  const [, openJoined] = useOpenJoined(room.account, noop)
  // An edit's text, apart from the draft, which it leaves as it is: as
  // the server made it of the message (`source`), and as typed since.
  const [edit, setEdit] = useState<{ event: number; source: string; text: string } | null>(null)
  const picker = useRef<HTMLInputElement>(null)
  const editing = mode.type === 'edit' ? mode.event : null
  // Until the server makes it, the edit has no text to write to.
  const loaded = editing && edit?.event === editing.id ? edit : null
  const value = editing ? (loaded?.text ?? '') : draft
  // Files still uploading are waited for.
  const sendable = editing
    ? loaded !== null && value.trim() !== ''
    : attaching.length === 0 && (value.trim() !== '' || files.length > 0)
  const typing = useTyping(room)
  const typed = completing(value, caret)
  const open = dismissed ? null : typed
  const suggestions = useSuggestions(room, me, open)
  const choice = Math.min(selected, suggestions.length - 1)
  // `:code:` typed whole becomes its emoji, as in Element.
  const emoji = useEmoji(value.includes(':'))

  // A room's box, or a thread's, has the focus once it shows: typing is
  // what one does there. Esc leaves it, to the shortcuts.
  useLayoutEffect(() => focusShown(props.textarea.current!), [props.textarea])
  useKeymap(props.scrolling, { within: props.textarea })
  const editingId = editing?.id ?? null
  useEffect(() => {
    if (editingId === null) return
    let current = true
    matrix.editText(room.id, editingId).then(
      (text) => current && setEdit({ event: editingId, source: text, text }),
      (e) => current && setError((e as Error).message),
    )
    return () => {
      current = false
    }
  }, [matrix, room.id, editingId])
  // What is typed and not yet saved is, on leaving the box.
  useEffect(() => () => void session.flush(), [session])
  // Edits from elsewhere, rebased into what is typed here: the caret
  // stays where it was in the text.
  useEffect(() => {
    session.onRebase = (_, from, to) => {
      const box = props.textarea.current!
      if (editingId !== null || document.activeElement !== box) return
      const at = movePosition(box.selectionStart, diff(from, to)!)
      setCaret(at)
      placeCaret.current = at
    }
    return () => {
      session.onRebase = null
    }
  }, [session, editingId, props.textarea])
  useLayoutEffect(() => {
    if (placeCaret.current === null) return
    props.textarea.current!.setSelectionRange(placeCaret.current, placeCaret.current)
    placeCaret.current = null
  }, [value, props.textarea])

  /** Sets the box's text, and where the caret goes. */
  const write = (text: string, at: number) => {
    if (editing) setEdit({ ...loaded!, text })
    else session.set('text', text)
    setError(null)
    setCaret(at)
    placeCaret.current = at
  }
  const choose = (suggestion: Suggestion) => {
    write(value.slice(0, open!.start) + suggestion.insert + value.slice(caret), open!.start + suggestion.insert.length)
    setSelected(0)
  }

  const leave = () => {
    setEdit(null)
    setMode({ type: 'new' })
  }
  /** At once: what is sent shows in the room, going out, or why it was
   * not sent. What waits for the server (joining, leaving) clears the box
   * too, giving a command refused back if the box is still empty. */
  const send = () => {
    if (!sendable) return
    typing.stop()
    if (editing) {
      if (value !== loaded!.source) {
        const command = parse(value)
        if (command.type === 'error') {
          setError(command.message)
          return
        }
        if (command.type !== 'message') {
          setError('An edit is of a message, not a command. To send a /, begin with //.')
          return
        }
        // It stays an action, as `/me` makes it one.
        const emote = command.emote || shown(editing).msgtype === 'm.emote'
        const { body, format } = command
        matrix.send(room.id, message(body, { format, emote, replaces: editing.event_id }))
      }
      leave()
      return
    }
    // The text answers what it replies to; else the first file does.
    let reply_to = mode.type === 'reply' ? mode.event.event_id : null
    if (value.trim() !== '') {
      const command = parse(value)
      switch (command.type) {
        case 'error':
          setError(command.message)
          return
        case 'message': {
          const { body, format, emote } = command
          matrix.send(room.id, message(body, { format, emote, reply_to, thread }))
          reply_to = null
          break
        }
        case 'member':
        case 'name':
        case 'topic':
          matrix.send(room.id, command)
          break
        case 'join': {
          const given = value
          matrix.join({ type: 'join_matrix_room', account: room.account, room: command.room }).then(openJoined, (e) => {
            if (session.content.value.text === '') session.set('text', given)
            setError((e as Error).message)
          })
          break
        }
        case 'leave':
          void matrix.changeMembership(room.id, 'leave')
          break
      }
      // Emptied as it was filled, by an edit: saved at once, if what
      // was sent was saved.
      session.set('text', '')
      void session.flush()
      // Files go with a message, not with other commands.
      if (command.type !== 'message') return
    }
    setMode({ type: 'new' })
    for (const { id, blob, name, mime } of files) {
      matrix.send(room.id, { type: 'file', blob, name, mime, reply_to })
      // Only the first answers what it replies to.
      reply_to = null
      void matrix.drafts.detach(room.id, id)
    }
  }

  return (
    <form
      class={styles.composer}
      onSubmit={(event) => {
        event.preventDefault()
        send()
      }}
    >
      {mode.type !== 'new' && (
        <div class={styles.mode}>
          {mode.type === 'reply' ? (
            <span>
              Replying to{' '}
              <span style={{ color: senderColor(mode.event.sender) }}>
                {displayName(mode.event.sender, mode.event.sender_name)}
              </span>
              : <span class={styles.quoted}>{shown(mode.event).body}</span>
            </span>
          ) : (
            <span>Editing your message</span>
          )}
          <button type="button" aria-label="Cancel" title="Cancel (Esc)" onMouseDown={keepFocus} onClick={leave}>
            ✕
          </button>
        </div>
      )}
      {open !== null && suggestions.length > 0 && (
        <Suggestions suggestions={suggestions} selected={choice} choose={choose} />
      )}
      {(error ?? session.error.value) !== null && (
        <p class={styles.composerError} role="alert">
          {error ?? session.error.value}
        </p>
      )}
      {files.length + attaching.length > 0 && editing === null && (
        <ul class={styles.staged} aria-label="Files to send">
          {files.map((file) => (
            <li key={file.id}>
              <Icon name="paperclip" />
              {file.name}
              <button
                type="button"
                aria-label={`Remove ${file.name}`}
                onMouseDown={keepFocus}
                onClick={() => void matrix.drafts.detach(room.id, file.id)}
              >
                ✕
              </button>
            </li>
          ))}
          {attaching.map((file, i) => (
            <li key={`attaching ${i}`} aria-busy="true">
              <Icon name="paperclip" />
              {file.name}…
            </li>
          ))}
        </ul>
      )}
      <div class={styles.input}>
        {editing === null && attachable && (
          <>
            <button
              type="button"
              class={styles.attach}
              aria-label="Attach files"
              title="Attach files"
              onMouseDown={keepFocus}
              onClick={() => picker.current!.click()}
            >
              <Icon name="paperclip" />
            </button>
            <input
              ref={picker}
              type="file"
              multiple
              hidden
              aria-label="Files"
              onChange={(event) => {
                attach([...event.currentTarget.files!])
                event.currentTarget.value = ''
                props.textarea.current!.focus()
              }}
            />
          </>
        )}
        <textarea
          ref={props.textarea}
          // What the history gives the focus back to (`restoreFocus`).
          id={thread === null ? 'message-box' : 'thread-box'}
          aria-label={thread !== null ? 'Reply in thread' : `Message ${room.name}`}
          placeholder={editing ? 'Edit your message' : thread !== null ? 'Reply in thread' : `Message ${room.name}`}
          rows={1}
          value={value}
          readOnly={editing !== null && loaded === null}
          onInput={(event) => {
            const box = event.currentTarget
            let text = box.value
            let at = box.selectionStart
            const code = /(^|\s):([\w+-]+):$/.exec(text.slice(0, at))
            const found = code && emoji ? byShortcode(emoji, code[2]) : undefined
            if (code && found) {
              const start = at - code[2].length - 2
              text = text.slice(0, start) + found.unicode + text.slice(at)
              at = start + found.unicode.length
            }
            write(text, at)
            setSelected(0)
            setDismissed(false)
            if (text.trim() === '') typing.stop()
            else typing.typed()
          }}
          onKeyUp={(event) => setCaret(event.currentTarget.selectionStart)}
          onClick={(event) => setCaret(event.currentTarget.selectionStart)}
          onPaste={(event) => {
            const pasted = [...(event.clipboardData?.files ?? [])]
            if (pasted.length === 0 || editing || !attachable) return
            event.preventDefault()
            attach(pasted)
          }}
          onKeyDown={(event) => {
            if (event.isComposing) return
            if (open !== null && suggestions.length > 0) {
              const key = event.key
              if (key === 'ArrowDown' || key === 'ArrowUp') {
                event.preventDefault()
                const step = key === 'ArrowDown' ? 1 : -1
                setSelected((choice + step + suggestions.length) % suggestions.length)
                return
              }
              if (key === 'Tab' || (key === 'Enter' && !event.shiftKey)) {
                event.preventDefault()
                choose(suggestions[choice])
                return
              }
              if (key === 'Escape') {
                event.preventDefault()
                setDismissed(true)
                return
              }
            }
            // Enter sends; Shift+Enter is a new line.
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault()
              send()
            } else if (event.key === 'ArrowUp' && value === '' && !editing && props.lastOwn) {
              event.preventDefault()
              setMode({ type: 'edit', event: props.lastOwn, back: false })
            } else if (event.key === 'Escape') {
              if (mode.type !== 'new') {
                leave()
                if (mode.back) props.focusLine(mode.event.id)
              } else if (props.back) props.back()
              else event.currentTarget.blur()
            }
          }}
        />
        <button type="submit" disabled={!sendable}>
          {editing ? 'Save' : 'Send'}
        </button>
      </div>
    </form>
  )
}

function noop() {}

/** Tells the room while the account types: renewed as it goes on, and
 * stopped once the box is emptied or sent, or the room left. */
function useTyping(room: Room) {
  const { matrix } = useServices()
  // When the last notice went, while typing.
  const since = useRef<number | null>(null)
  const stop = () => {
    if (since.current === null) return
    since.current = null
    matrix.setTyping(room.id, false)
  }
  useEffect(
    () => () => {
      if (since.current !== null) matrix.setTyping(room.id, false)
    },
    [matrix, room.id],
  )
  return {
    typed: () => {
      const now = Date.now()
      if (since.current !== null && now - since.current < TYPING_RENEW) return
      since.current = now
      matrix.setTyping(room.id, true)
    },
    stop,
  }
}
