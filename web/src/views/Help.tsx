import { KEYMAP, useEscape } from '../keymap'
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
