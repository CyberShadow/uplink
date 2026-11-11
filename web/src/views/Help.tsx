import { KEYMAP, pendingSequence, useEscape } from '../keymap'
import styles from './Overlay.module.css'

export function Help({ close }: { close: () => void }) {
  useEscape(close)
  return (
    <div class={styles.backdrop} onClick={close}>
      <div class={styles.dialog} role="dialog" aria-label="Keyboard shortcuts">
        <h2>Keyboard shortcuts</h2>
        <table class={styles.keys}>
          <tbody>
            {KEYMAP.map((binding) => (
              <tr key={binding.action}>
                <td>
                  <kbd>{binding.label}</kbd>
                </td>
                <td>{binding.help}</td>
              </tr>
            ))}
          </tbody>
        </table>
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
