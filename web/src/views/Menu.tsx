// A button opening a menu of actions, as Gmail's "⋮" on a message does.

import { useEffect, useRef, useState } from 'preact/hooks'
import { keepFocus, useEscape } from '../keymap'
import { Icon, type IconName } from './Icon'
import styles from './Menu.module.css'
import { useOpensAbove } from './popover'

export interface MenuItem {
  label: string
  icon?: IconName
  action: () => void
}

/** `groups`: the items, separated between groups. The menu opens below
 * the button (above, if cut off there: `useOpensAbove`), aligned with its end, or with `start`, its start, as a
 * button at the start of a bar needs. The button is its `icon`, or
 * without one, a button of its label, as in a line of text. */
export function MenuButton(props: { label: string; icon?: IconName; groups: MenuItem[][]; start?: true }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLSpanElement>(null)
  const menu = useRef<HTMLUListElement>(null)
  const above = useOpensAbove(menu, open)
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
        class={props.icon && styles.button}
        aria-label={props.icon && props.label}
        title={props.icon && props.label}
        aria-haspopup="menu"
        aria-expanded={open}
        onMouseDown={keepFocus}
        onClick={() => setOpen(!open)}
      >
        {props.icon ? <Icon name={props.icon} /> : `${props.label} ▾`}
      </button>
      {open && (
        <ul
          ref={menu}
          class={[styles.menu, props.start && styles.start, above && styles.above].filter(Boolean).join(' ')}
          role="menu"
          aria-label={props.label}
        >
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
