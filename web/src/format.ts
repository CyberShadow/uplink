// Dates and times, one way throughout (SPEC §12.9): YYYY-MM-DD and 24-hour
// HH:MM, in the browser's time zone, whatever its locale's conventions.

const pad = (n: number) => String(n).padStart(2, '0')

/** `YYYY-MM-DD` of the local day `day` is in. */
export function date(day: Date): string {
  return `${day.getFullYear()}-${pad(day.getMonth() + 1)}-${pad(day.getDate())}`
}

/** `HH:MM`, of `ms` since the epoch. */
export function time(ms: number): string {
  const t = new Date(ms)
  return `${pad(t.getHours())}:${pad(t.getMinutes())}`
}

/** `YYYY-MM-DD HH:MM`, of `ms` since the epoch. */
export function dateTime(ms: number): string {
  return `${date(new Date(ms))} ${time(ms)}`
}

/** For a list: the time if `ms` is today, else the date. */
export function brief(ms: number, now = new Date()): string {
  const day = new Date(ms)
  return date(day) === date(now) ? time(ms) : date(day)
}
