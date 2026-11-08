import { useEffect } from 'preact/hooks'
import { tinykeys } from 'tinykeys'
import { KEYMAP } from '../keymap'
import styles from './Overlay.module.css'

export function Help(props: { close: () => void }) {
  useEffect(() => tinykeys(window, { Escape: props.close }), [])
  return (
    <div class={styles.backdrop} onClick={props.close}>
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
