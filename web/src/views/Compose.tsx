// The compose window (SPEC §8.5): address fields, subject, and a TipTap
// editor for the body. TipTap's core is framework-agnostic, so it is used
// directly rather than through its React bindings (SPEC §15.1). Loaded
// lazily: it is most of the bundle.

import { Editor } from '@tiptap/core'
import StarterKit from '@tiptap/starter-kit'
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Draft, DraftField } from '../generated/protocol'
import type { ComposeSession } from '../compose/session'
import { diff, movePosition } from '../compose/splice'
import { useServices } from '../context'
import { defaultAddress, identityOf } from '../identities'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Compose.module.css'
import { fileSize } from './size'
import { useTitle } from '../title'

export default function Compose(props: { draft?: string }) {
  const { drafts } = useServices()
  const loaded = drafts.drafts.value
  const draft = loaded?.get(props.draft!)
  const session = draft && drafts.session(draft.id)
  useTitle(...(session ? [session.content.value.subject || '(no subject)', 'Draft'] : []))
  if (!loaded) return <p class={styles.placeholder}>Loading…</p>
  if (!draft) return <p class={styles.placeholder}>This draft was sent or discarded.</p>
  return <ComposeForm session={session!} draft={draft} />
}

const ADDRESS_FIELDS: [DraftField, string][] = [
  ['to', 'To'],
  ['cc', 'Cc'],
  ['bcc', 'Bcc'],
]

function ComposeForm(props: { session: ComposeSession; draft: Draft }) {
  const { session, draft } = props
  const { drafts, mail } = useServices()
  const { route } = useLocation()
  const form = useRef<HTMLFormElement>(null)
  const inputs = useRef(new Map<DraftField, HTMLInputElement>())
  const editor = useRef<Editor | null>(null)
  const body = useRef<HTMLDivElement>(null)
  const content = session.content.value
  const gone = session.state.value === 'gone'
  const picker = useRef<HTMLInputElement>(null)
  const [uploading, setUploading] = useState(0)
  const attach = async (files: readonly File[]) => {
    setUploading((n) => n + files.length)
    try {
      await drafts.attach(session.id, files)
    } finally {
      setUploading((n) => n - files.length)
    }
  }

  const close = () => {
    session.flush()
    if (history.length > 1) history.back()
    else route('/')
  }
  const send = async () => {
    if (uploading > 0) return
    await session.flush()
    if (session.state.value === 'gone') return
    if (await drafts.send(session.id)) close()
  }
  const discard = async () => {
    if (await drafts.discard(session.id)) close()
  }
  useKeymap({ send, close }, { within: form })

  // Before paint, so that keys typed at once go where writing starts.
  useLayoutEffect(() => {
    const instance = new Editor({
      element: body.current!,
      extensions: [StarterKit],
      content: session.content.peek().body,
      onUpdate: ({ editor }) => session.set('body', editor.getHTML()),
    })
    editor.current = instance
    // Where writing starts: the recipients, or for a reply, the body, whose
    // selection starts at its start. Not by tiptap's `focus`, which waits
    // for the next frame: a key pressed before it would miss the editor.
    if (session.content.peek().to === '') inputs.current.get('to')!.focus()
    else instance.view.focus()
    return () => instance.destroy()
  }, [session])

  useEffect(() => {
    // Without an update: it would send the editor's HTML as an edit, which
    // for an empty draft is `<p></p>`, from every tab that opens it.
    editor.current!.setEditable(!gone, false)
  }, [gone])

  // Edits from elsewhere, rebased into what is typed here.
  useEffect(() => {
    session.onRebase = (field, from, to) => {
      if (field === 'body') {
        const instance = editor.current!
        const { from: start, to: end } = instance.state.selection
        instance.commands.setContent(to, { emitUpdate: false })
        const size = instance.state.doc.content.size
        instance.commands.setTextSelection({ from: Math.min(start, size), to: Math.min(end, size) })
        return
      }
      const input = inputs.current.get(field)!
      const edit = diff(from, to)!
      const [start, end] = [input.selectionStart ?? 0, input.selectionEnd ?? 0]
      input.value = to
      if (document.activeElement === input) {
        input.setSelectionRange(movePosition(start, edit), movePosition(end, edit))
      }
    }
    return () => {
      session.onRebase = null
    }
  }, [session])

  const identities = mail.accounts.value?.find((a) => a.id === session.account)?.identities ?? []
  const listId = `identities-${session.id}`
  const field = (name: DraftField, label: string, extra: { placeholder?: string; list?: string } = {}) => (
    <label key={name} class={styles.field}>
      <span>{label}</span>
      <input
        ref={(el) => {
          if (el) inputs.current.set(name, el)
        }}
        type="text"
        value={content[name]}
        disabled={gone}
        onInput={(event) => session.set(name, event.currentTarget.value)}
        {...extra}
      />
    </label>
  )
  // Whom it is from: an identity's address, or empty for the account's
  // own (SPEC §3.2); the name it goes under is the identity's.
  const from = content.from.trim()
  const fromIdentity = identities.length > 0 ? identityOf(identities, from || defaultAddress(identities)) : undefined

  return (
    <form
      ref={form}
      class={styles.compose}
      aria-label="Compose"
      onSubmit={(event) => {
        event.preventDefault()
        send()
      }}
      onDragOver={(event) => {
        if (!gone && event.dataTransfer?.types.includes('Files')) event.preventDefault()
      }}
      onDrop={(event) => {
        const files = event.dataTransfer?.files
        if (gone || !files?.length) return
        event.preventDefault()
        attach([...files])
      }}
    >
      {field('from', 'From', { placeholder: identities.length > 0 ? defaultAddress(identities) : '', list: listId })}
      <datalist id={listId}>
        {identities.flatMap((i) => ('address' in i ? [<option key={i.address} value={i.address} label={i.name ?? undefined} />] : []))}
      </datalist>
      {identities.length > 0 && (
        <div class={fromIdentity ? styles.from : `${styles.from} ${styles.error}`} aria-live="polite">
          {fromIdentity
            ? fromIdentity.name && `As ${fromIdentity.name}`
            : 'Not one of the account’s addresses'}
        </div>
      )}
      {ADDRESS_FIELDS.map(([name, label]) => field(name, label))}
      {field('subject', 'Subject')}
      <div class={styles.body} ref={body} aria-label="Body" />
      {draft.attachments.length > 0 && (
        <ul class={styles.attachments} aria-label="Attachments">
          {draft.attachments.map((a) => (
            <li key={a.id}>
              {a.name} <span class={styles.size}>{fileSize(a.size)}</span>
              <button
                type="button"
                aria-label={`Remove ${a.name}`}
                title="Remove"
                onMouseDown={keepFocus}
                onClick={() => drafts.detach(session.id, a.id)}
                disabled={gone}
              >
                ×
              </button>
            </li>
          ))}
        </ul>
      )}
      <div class={styles.actions}>
        <button type="submit" disabled={gone || uploading > 0}>
          Send
        </button>
        <button type="button" onMouseDown={keepFocus} onClick={() => picker.current!.click()} disabled={gone}>
          Attach
        </button>
        <input
          ref={picker}
          type="file"
          multiple
          hidden
          aria-label="Files to attach"
          onChange={(event) => {
            const input = event.currentTarget
            const files = [...(input.files ?? [])]
            input.value = ''
            if (files.length) attach(files)
          }}
        />
        <button type="button" onMouseDown={keepFocus} onClick={discard} disabled={gone}>
          Discard
        </button>
        <span class={styles.state} aria-live="polite">
          {gone
            ? 'Sent or discarded elsewhere'
            : uploading > 0
              ? 'Attaching…'
              : session.state.value === 'saved'
                ? 'Saved'
                : 'Saving…'}
        </span>
        {session.error.value && <span class={styles.error}>{session.error.value}</span>}
      </div>
    </form>
  )
}
