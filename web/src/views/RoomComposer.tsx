// The box to write to a room or one of its threads (SPEC §12.11):
// Markdown, and commands as Element's (`/me`, `/plain`, `/invite`…), with
// completions of commands, members and emoji; a reply to a message, or an
// edit of one's own (Up edits the last); and, but in a thread, files,
// attached, pasted or dropped, sent with it.

import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import type { Room, RoomEvent } from '../generated/protocol'
import { parse } from '../commands'
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
export type Mode = { type: 'new' } | { type: 'reply'; event: RoomEvent } | { type: 'edit'; event: RoomEvent }

/** How often a typing notice is renewed while typing: the homeserver
 * forgets it after 4 s, as the SDK sends it. */
const TYPING_RENEW = 3000

export function Composer(props: {
  room: Room
  textarea: preact.RefObject<HTMLTextAreaElement>
  mode: Mode
  setMode: (mode: Mode) => void
  /** The account's last message of text, which Up edits. */
  lastOwn: RoomEvent | null
  /** Of a thread's box, the thread's root (a Matrix event id). */
  thread: string | null
  /** Files to send with the message, but in a thread. */
  attachments: { files: readonly File[]; setFiles: (files: File[]) => void } | null
  /** Scrolls what the box writes to, by the page keys, from the box. */
  scrolling: Actions
}) {
  const { room, mode, setMode, thread } = props
  const files = props.attachments?.files ?? NO_FILES
  const { matrix } = useServices()
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const [draft, setDraft] = useState('')
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
  // An edit's text, apart from the draft, which it leaves as it is.
  const [edit, setEdit] = useState<{ event: number; text: string } | null>(null)
  const picker = useRef<HTMLInputElement>(null)
  const editing = mode.type === 'edit' ? mode.event : null
  const value = editing ? (edit?.event === editing.id ? edit.text : (shown(editing).body ?? '')) : draft
  const sendable = value.trim() !== '' || (editing === null && files.length > 0)
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
  useLayoutEffect(() => {
    if (placeCaret.current === null) return
    props.textarea.current!.setSelectionRange(placeCaret.current, placeCaret.current)
    placeCaret.current = null
  }, [value, props.textarea])

  /** Sets the box's text, and where the caret goes. */
  const write = (text: string, at: number) => {
    if (editing) setEdit({ event: editing.id, text })
    else setDraft(text)
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
  const send = async () => {
    if (!sendable) return
    typing.stop()
    if (editing) {
      const original = shown(editing)
      if (value !== original.body) {
        const content = message(value, { emote: original.msgtype === 'm.emote', replaces: editing.event_id })
        if (!(await matrix.send(room.id, content))) return
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
          const { body, markdown, emote } = command
          if (!(await matrix.send(room.id, message(body, { markdown, emote, reply_to, thread })))) return
          reply_to = null
          break
        }
        case 'member':
        case 'name':
        case 'topic':
          if (!(await matrix.send(room.id, command))) return
          break
        case 'join':
          try {
            openJoined(await matrix.join({ type: 'join_matrix_room', account: room.account, room: command.room }))
          } catch (e) {
            setError((e as Error).message)
            return
          }
          break
        case 'leave':
          if (!(await matrix.changeMembership(room.id, 'leave'))) return
          break
      }
      setDraft('')
      // Files go with a message, not with other commands.
      if (command.type !== 'message') return
    }
    setMode({ type: 'new' })
    if (files.length === 0) return
    props.attachments!.setFiles([])
    await matrix.sendFiles(room.id, files, reply_to)
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
      {error !== null && (
        <p class={styles.composerError} role="alert">
          {error}
        </p>
      )}
      {files.length > 0 && editing === null && (
        <ul class={styles.staged} aria-label="Files to send">
          {files.map((file, i) => (
            <li key={i}>
              <Icon name="paperclip" />
              {file.name}
              <button
                type="button"
                aria-label={`Remove ${file.name}`}
                onMouseDown={keepFocus}
                onClick={() => props.attachments!.setFiles(files.filter((_, j) => j !== i))}
              >
                ✕
              </button>
            </li>
          ))}
        </ul>
      )}
      <div class={styles.input}>
        {editing === null && props.attachments !== null && (
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
                props.attachments!.setFiles([...files, ...event.currentTarget.files!])
                event.currentTarget.value = ''
                props.textarea.current!.focus()
              }}
            />
          </>
        )}
        <textarea
          ref={props.textarea}
          aria-label={thread !== null ? 'Reply in thread' : `Message ${room.name}`}
          placeholder={editing ? 'Edit your message' : thread !== null ? 'Reply in thread' : `Message ${room.name}`}
          rows={1}
          value={value}
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
            if (pasted.length === 0 || editing || props.attachments === null) return
            event.preventDefault()
            props.attachments.setFiles([...files, ...pasted])
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
              setMode({ type: 'edit', event: props.lastOwn })
            } else if (event.key === 'Escape') {
              if (mode.type !== 'new') leave()
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

const NO_FILES: readonly File[] = []

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
