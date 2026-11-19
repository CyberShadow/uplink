// Asks before an action that takes away, from others, what they have,
// which no undo gives back once the provider has it (SPEC §6.8): its
// button has the focus, so Enter does it; Keep, Esc or a click outside
// does not.

import { useRef } from 'preact/hooks'
import { useEscape, useTakeFocus } from '../keymap'
import overlay from './Overlay.module.css'
import { Backdrop } from './Backdrop'

export function Confirm(props: { title: string; text: string; action: string; confirm: () => void; close: () => void }) {
  const button = useRef<HTMLButtonElement>(null)
  useTakeFocus(button)
  useEscape(props.close)
  return (
    <Backdrop class={overlay.backdrop} close={props.close}>
      <div class={overlay.dialog} role="alertdialog" aria-label={props.title}>
        <h2>{props.title}?</h2>
        <p>{props.text}</p>
        <div class={overlay.buttons}>
          <button type="button" onClick={props.close}>
            Keep
          </button>
          <button
            ref={button}
            type="button"
            onClick={() => {
              props.close()
              props.confirm()
            }}
          >
            {props.action}
          </button>
        </div>
      </div>
    </Backdrop>
  )
}
