// A button opening a menu of actions, as Gmail's "⋮" on a message does.

import { useEffect, useRef, useState } from 'preact/hooks'
import { keepFocus, useEscape } from '../keymap'
import { Icon, type IconName } from './Icon'
import styles from './Menu.module.css'

export interface MenuItem {
  label: string
  icon?: IconName
  action: () => void
}

/** `groups`: the items, separated between groups. The menu opens below
 * the button, aligned with its end, or with `start`, its start, as a
 * button at the start of a bar needs. */
export function MenuButton(props: { label: string; icon: IconName; groups: MenuItem[][]; start?: true }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLSpanElement>(null)
  useEffect(() => {
    if (!open) return
    // Anywhere else closes it, as Escape does.
    const outside = (event: PointerEvent) => {
      if (!root.current!.contains(event.target as Node)) setOpen(false)
    }
    document.addEventListener('pointerdown', outside)
    return () => document.removeEventListener('pointerdown', outside)
  }, [open])
  useEscape(open ? () => setOpen(false) : null)
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
        <ul class={[styles.menu, props.start && styles.start].filter(Boolean).join(' ')} role="menu" aria-label={props.label}>
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
                  {item.icon && <Icon name={item.icon} />}
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
