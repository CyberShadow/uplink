// Dates and times, one way throughout (SPEC §12.10): YYYY-MM-DD and 24-hour
// HH:MM, in the browser's time zone, whatever its locale's conventions;
// lists abbreviate this year's dates.

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

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/** For a list, as Gmail's: the time if `ms` is today, the day and month
 * (`9 Nov`) if this year, else the date. */
export function brief(ms: number, now = new Date()): string {
  const day = new Date(ms)
  if (date(day) === date(now)) return time(ms)
  if (day.getFullYear() === now.getFullYear()) return `${day.getDate()} ${MONTHS[day.getMonth()]}`
  return date(day)
}
