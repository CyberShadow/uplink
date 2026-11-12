import styles from './Badge.module.css'

/** How a count shows (SPEC §12.14): `alert`, in red, of what needs the
 * user's attention (mentions, the count buckets' items); `unread`, in
 * grey, of what is unread. A size, how many there are, is no badge but
 * a plain number. */
export type BadgeTone = 'alert' | 'unread'

/** A count in a pill, at the end of a row or a nav place, whole (only
 * the favicon's, of a few pixels, stops at `99+`); none of 0. */
export function Badge(props: { count: number; tone: BadgeTone; title: string; prefix?: string }) {
  if (props.count === 0) return null
  return (
    <span class={`${styles.badge} ${styles[props.tone]}`} title={props.title}>
      {props.prefix}
      {props.count.toLocaleString()}
    </span>
  )
}
