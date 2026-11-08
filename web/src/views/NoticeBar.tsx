import { useEffect } from 'preact/hooks'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import styles from './Overlay.module.css'

const SHOWN_MS = 10_000

/** The outcome of the last op, with its undo. */
export function NoticeBar() {
  const { mail } = useServices()
  const notice = mail.notice.value
  useEffect(() => {
    if (!notice) return
    const hide = setTimeout(() => {
      if (mail.notice.value === notice) mail.notice.value = null
    }, SHOWN_MS)
    return () => clearTimeout(hide)
  }, [notice])

  if (!notice) return null
  return (
    <div class={styles.notice} role="status">
      {notice.kind === 'done' ? (
        <>
          <span>{notice.description}</span>
          <button type="button" onMouseDown={keepFocus} onClick={() => mail.undo(notice.id)}>
            Undo
          </button>
        </>
      ) : (
        <span>Failed: {notice.message}</span>
      )}
      <button
        type="button"
        aria-label="Dismiss"
        onMouseDown={keepFocus}
        onClick={() => (mail.notice.value = null)}
      >
        ×
      </button>
    </div>
  )
}
