// The compose window (SPEC §8.5): address fields, subject, and a TipTap
// editor for the body. TipTap's core is framework-agnostic, so it is used
// directly rather than through its React bindings (SPEC §15.1). Loaded
// lazily: it is most of the bundle.

import { Editor } from '@tiptap/core'
import StarterKit from '@tiptap/starter-kit'
import { useEffect, useLayoutEffect, useRef } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { DraftField } from '../generated/protocol'
import type { ComposeSession } from '../compose/session'
import { diff, movePosition } from '../compose/splice'
import { useServices } from '../context'
import { keepFocus, useKeymap } from '../keymap'
import styles from './Compose.module.css'

export default function Compose(props: { draft?: string }) {
  const { drafts } = useServices()
  const loaded = drafts.drafts.value
  const draft = loaded?.get(props.draft!)
  if (!loaded) return <p class={styles.placeholder}>Loading…</p>
  if (!draft) return <p class={styles.placeholder}>This draft was sent or discarded.</p>
  return <ComposeForm session={drafts.session(draft.id)} />
}

const ADDRESS_FIELDS: [DraftField, string][] = [
  ['to', 'To'],
  ['cc', 'Cc'],
  ['bcc', 'Bcc'],
]

function ComposeForm(props: { session: ComposeSession }) {
  const { session } = props
  const { drafts, mail } = useServices()
  const { route } = useLocation()
  const form = useRef<HTMLFormElement>(null)
  const inputs = useRef(new Map<DraftField, HTMLInputElement>())
  const editor = useRef<Editor | null>(null)
  const body = useRef<HTMLDivElement>(null)
  const content = session.content.value
  const gone = session.state.value === 'gone'

  const close = () => {
    session.flush()
    if (history.length > 1) history.back()
    else route('/')
  }
  const send = async () => {
    await session.flush()
    if (session.state.value === 'gone') return
    if (await drafts.send(session.id)) close()
  }
  const discard = async () => {
    if (await drafts.discard(session.id)) close()
  }
  useKeymap({ send, close }, form)

  // Before paint, so that keys typed at once go where writing starts.
  useLayoutEffect(() => {
    const instance = new Editor({
      element: body.current!,
      extensions: [StarterKit],
      content: session.content.peek().body,
      onUpdate: ({ editor }) => session.set('body', editor.getHTML()),
    })
    editor.current = instance
    // Where writing starts: the recipients, or for a reply, the body.
    if (session.content.peek().to === '') inputs.current.get('to')!.focus()
    else instance.commands.focus('start')
    return () => instance.destroy()
  }, [session])

  useEffect(() => {
    editor.current!.setEditable(!gone)
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

  const from = mail.accounts.value?.find((a) => a.id === session.account)?.address ?? session.account
  const field = (name: DraftField, label: string) => (
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
      />
    </label>
  )

  return (
    <form
      ref={form}
      class={styles.compose}
      aria-label="Compose"
      onSubmit={(event) => {
        event.preventDefault()
        send()
      }}
    >
      <div class={styles.from}>From {from}</div>
      {ADDRESS_FIELDS.map(([name, label]) => field(name, label))}
      {field('subject', 'Subject')}
      <div class={styles.body} ref={body} aria-label="Body" />
      <div class={styles.actions}>
        <button type="submit" disabled={gone}>
          Send
        </button>
        <button type="button" onMouseDown={keepFocus} onClick={discard} disabled={gone}>
          Discard
        </button>
        <span class={styles.state} aria-live="polite">
          {gone ? 'Sent or discarded elsewhere' : session.state.value === 'saved' ? 'Saved' : 'Saving…'}
        </span>
        {session.error.value && <span class={styles.error}>{session.error.value}</span>}
      </div>
    </form>
  )
}
