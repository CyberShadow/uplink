// The box to write to a room (SPEC §12.11): Markdown, `/me` for an emote;
// a reply to a message, or an edit of one's own (Up edits the last);
// and files, attached, pasted or dropped, sent with it.

import { useRef, useState } from 'preact/hooks'
import type { Room, RoomEvent } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import { displayName, senderColor, shown, text as message } from '../matrix'
import { Icon } from './Icon'
import styles from './Room.module.css'

/** What the composer writes: a message, a reply to one, or an edit of
 * one of the account's own. */
export type Mode = { type: 'new' } | { type: 'reply'; event: RoomEvent } | { type: 'edit'; event: RoomEvent }

const EMOTE = '/me '

export function Composer(props: {
  room: Room
  textarea: preact.RefObject<HTMLTextAreaElement>
  mode: Mode
  setMode: (mode: Mode) => void
  /** The account's last message of text, which Up edits. */
  lastOwn: RoomEvent | null
  /** Files to send with the message. */
  files: readonly File[]
  setFiles: (files: File[]) => void
}) {
  const { room, mode, setMode, files, setFiles } = props
  const { matrix } = useServices()
  const [draft, setDraft] = useState('')
  // An edit's text, apart from the draft, which it leaves as it is.
  const [edit, setEdit] = useState<{ event: number; text: string } | null>(null)
  const picker = useRef<HTMLInputElement>(null)
  const editing = mode.type === 'edit' ? mode.event : null
  const value = editing ? (edit?.event === editing.id ? edit.text : (shown(editing).body ?? '')) : draft
  const sendable = value.trim() !== '' || (editing === null && files.length > 0)

  const leave = () => {
    setEdit(null)
    setMode({ type: 'new' })
  }
  const send = async () => {
    if (!sendable) return
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
      const emote = value.startsWith(EMOTE)
      if (!(await matrix.send(room.id, message(emote ? value.slice(EMOTE.length) : value, { emote, reply_to })))) return
      reply_to = null
      setDraft('')
    }
    const sent = files
    setFiles([])
    setMode({ type: 'new' })
    await matrix.sendFiles(room.id, sent, reply_to)
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
                onClick={() => setFiles(files.filter((_, j) => j !== i))}
              >
                ✕
              </button>
            </li>
          ))}
        </ul>
      )}
      <div class={styles.input}>
        {editing === null && (
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
                setFiles([...files, ...event.currentTarget.files!])
                event.currentTarget.value = ''
                props.textarea.current!.focus()
              }}
            />
          </>
        )}
        <textarea
          ref={props.textarea}
          aria-label={`Message ${room.name}`}
          placeholder={editing ? 'Edit your message' : `Message ${room.name}`}
          rows={1}
          value={value}
          onInput={(event) => {
            const text = event.currentTarget.value
            if (editing) setEdit({ event: editing.id, text })
            else setDraft(text)
          }}
          onPaste={(event) => {
            const pasted = [...(event.clipboardData?.files ?? [])]
            if (pasted.length === 0 || editing) return
            event.preventDefault()
            setFiles([...files, ...pasted])
          }}
          onKeyDown={(event) => {
            if (event.isComposing) return
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
