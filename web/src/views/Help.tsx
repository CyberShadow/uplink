import { useEffect, useRef, useState } from 'preact/hooks'
import { ALT, type Binding, boundActions, KEYMAP, MOD, pendingSequence, useTakeFocus } from '../keymap'
import styles from './Overlay.module.css'

export function Help({ close }: { close: () => void }) {
  // Keys reach nothing behind it; Esc, or ? again, close it.
  // Focused, it scrolls by the keys that scroll (arrows, pages, Space),
  // which go on to it; once closed, the focus is back where it was.
  const dialog = useRef<HTMLDivElement>(null)
  useTakeFocus(dialog)
  useEffect(() => {
    const keys = (event: KeyboardEvent) => {
      event.stopPropagation()
      if (event.key !== 'Escape' && event.key !== '?') return
      event.preventDefault()
      close()
    }
    window.addEventListener('keydown', keys, true)
    return () => window.removeEventListener('keydown', keys, true)
  }, [close])
  // As bound when it opened, which is what it is shown over.
  const [here] = useState(boundActions)
  const rows = (bindings: Binding[]) => (
    <table class={styles.keys}>
      <tbody>
        {bindings.map((binding) => (
          <tr key={binding.action}>
            <td>
              <kbd>{binding.label}</kbd>
            </td>
            <td>{binding.help}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
  return (
    <div class={styles.backdrop} onClick={close}>
      <div
        ref={dialog}
        class={styles.dialog}
        role="dialog"
        aria-label="Keyboard shortcuts"
        tabIndex={-1}
        onClick={(e) => e.stopPropagation()}
      >
        <h2>Keyboard shortcuts</h2>
        <p class={styles.about}>
          Keys act on what is on show while nothing is being typed. A box being typed in keeps them, but for
          those with {MOD} or {ALT} held; <kbd>Esc</kbd> leaves it. A room's box has the focus as it opens:{' '}
          <kbd>Esc</kbd>, then keys; <kbd>i</kbd>, <kbd>Enter</kbd> or <kbd>r</kbd> go back to it. Pressed again
          and again, <kbd>Esc</kbd> goes up to home. After the first key of a sequence (<kbd>g</kbd>,{' '}
          <kbd>*</kbd>), a moment's wait shows what may follow.
        </p>
        <section aria-label="Here">
          <h3>Here</h3>
          {rows(KEYMAP.filter((b) => here.has(b.action)))}
        </section>
        <section aria-label="Elsewhere">
          <h3>Elsewhere</h3>
          {rows(KEYMAP.filter((b) => !here.has(b.action)))}
        </section>
      </div>
    </div>
  )
}

/** What may follow a sequence begun (`g`, `*`), once it has waited a
 * while for its next press, as Emacs' which-key shows. */
export function WhichKey() {
  const pending = pendingSequence.value
  if (!pending) return null
  return (
    <div class={styles.which} role="dialog" aria-label={`Keys after ${pending.typed}`}>
      <table class={styles.keys}>
        <tbody>
          {pending.next.map((next) => (
            <tr key={`${next.key} ${next.help}`}>
              <td>
                <kbd>
                  {pending.typed} {next.key}
                </kbd>
              </td>
              <td>{next.help}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
