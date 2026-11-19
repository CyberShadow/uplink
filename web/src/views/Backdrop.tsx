// What a dialog shows over (SPEC §12.5): a click on it, outside the
// dialog, closes the dialog, as Esc does by the keys.

import type { ComponentChildren } from 'preact'

export function Backdrop(props: { class: string; close: () => void; children?: ComponentChildren }) {
  return (
    // The pointer's way to close what Esc closes: no control of its own.
    // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions
    <div
      class={props.class}
      onClick={(event) => {
        // A click within the dialog is the dialog's, and goes no further.
        event.stopPropagation()
        if (event.target === event.currentTarget) props.close()
      }}
    >
      {props.children}
    </div>
  )
}
