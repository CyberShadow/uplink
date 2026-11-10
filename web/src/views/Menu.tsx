// A button opening a menu of actions, as Gmail's "⋮" on a message does.

import { useEffect, useRef, useState } from 'preact/hooks'
import { keepFocus } from '../keymap'
import { Icon, type IconName } from './Icon'
import styles from './Menu.module.css'

export interface MenuItem {
  label: string
  icon: IconName
  action: () => void
}

/** `groups`: the items, separated between groups. */
export function MenuButton(props: { label: string; icon: IconName; groups: MenuItem[][] }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLSpanElement>(null)
  useEffect(() => {
    if (!open) return
    // Anywhere else closes it, as Escape does (without the keys reaching
    // the shortcuts).
    const outside = (event: PointerEvent) => {
      if (!root.current!.contains(event.target as Node)) setOpen(false)
    }
    const escape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      event.stopPropagation()
      setOpen(false)
    }
    document.addEventListener('pointerdown', outside)
    window.addEventListener('keydown', escape, true)
    return () => {
      document.removeEventListener('pointerdown', outside)
      window.removeEventListener('keydown', escape, true)
    }
  }, [open])
  return (
    <span class={styles.root} ref={root}>
      <button
        type="button"
        class={styles.button}
        aria-label={props.label}
        title={props.label}
        aria-haspopup="menu"
        aria-expanded={open}
        onMouseDown={keepFocus}
        onClick={() => setOpen(!open)}
      >
        <Icon name={props.icon} />
      </button>
      {open && (
        <ul class={styles.menu} role="menu" aria-label={props.label}>
          {props.groups.flatMap((group, i) => [
            ...(i > 0 ? [<li key={`separator-${i}`} role="separator" class={styles.separator} />] : []),
            ...group.map((item) => (
              <li key={item.label} role="none">
                <button
                  type="button"
                  role="menuitem"
                  onMouseDown={keepFocus}
                  onClick={() => {
                    setOpen(false)
                    item.action()
                  }}
                >
                  <Icon name={item.icon} />
                  {item.label}
                </button>
              </li>
            )),
          ])}
        </ul>
      )}
    </span>
  )
}
