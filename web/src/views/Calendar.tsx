import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { Attendee, CalendarEvent, EventDetails, EventResponse, ShowAs } from '../generated/protocol'
import { useServices } from '../context'
import { useTitle } from '../title'
import { date as dateString, dateTime, time } from '../format'
import { keepFocus, useKeymap } from '../keymap'
import { linkify } from '../linkify'
import { HtmlBody } from './HtmlBody'
import styles from './Calendar.module.css'

const DAY = 86_400_000
const HOUR = 3_600_000
/** Days the agenda lists. */
const AGENDA_DAYS = 14
const HOUR_PX = 48
/** Events shorter than two lines are on one. */
const SHORT_PX = 32
/** The hour a week not of today opens at. */
const WORKDAY_START = 8

export type CalendarView = 'week' | 'agenda'

/** The view from `date`; with `event`, that event opened. */
export function calendarPath(view: CalendarView, date: string, event?: number): string {
  return `/calendar/${view}/${date}${event === undefined ? '' : `/${event}`}`
}

/** Local midnight of `YYYY-MM-DD`. */
function midnight(date: string): Date {
  const [y, m, d] = date.split('-').map(Number)
  return new Date(y, m - 1, d)
}

function addDays(day: Date, days: number): Date {
  return new Date(day.getFullYear(), day.getMonth(), day.getDate() + days)
}

export function today(): string {
  return dateString(new Date())
}

/** The Monday of the week of `day`. */
function weekStart(day: Date): Date {
  return addDays(day, -((day.getDay() + 6) % 7))
}

const dayName = (day: Date) => `${day.toLocaleDateString('en-GB', { weekday: 'long' })} ${dateString(day)}`
/** As a week's column heads it: `Tue 10`. */
const shortDayName = (day: Date) => `${day.toLocaleDateString('en-GB', { weekday: 'short' })} ${day.getDate()}`

/** The time, as of the minute: it changes as the minute does. */
function useNow(): number {
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>
    const tick = () => {
      setNow(Date.now())
      timer = setTimeout(tick, 60_000 - (Date.now() % 60_000))
    }
    timer = setTimeout(tick, 60_000 - (Date.now() % 60_000))
    return () => clearTimeout(timer)
  }, [])
  return now
}

/** Whether the event is on the (local) day: an all-day event by its UTC
 * dates, which are the days it is on wherever one is; a timed one if it
 * overlaps the day. */
function isOn(event: CalendarEvent, day: Date): boolean {
  if (event.all_day) {
    const utcDay = Date.UTC(day.getFullYear(), day.getMonth(), day.getDate())
    return event.start <= utcDay && utcDay < event.end
  }
  const start = day.getTime()
  const end = addDays(day, 1).getTime()
  return event.start < end && (event.end > start || (event.end === event.start && event.start >= start))
}

/** The week or agenda from `date`, of every account's calendar; with
 * `event`, that event opened beside it. */
export function Calendar(props: { view?: string; date?: string; event?: string }) {
  const { calendar } = useServices()
  const { route } = useLocation()
  const view: CalendarView = props.view === 'agenda' ? 'agenda' : 'week'
  const date = props.date ?? today()
  const opened = props.event === undefined ? null : Number(props.event)
  const first = view === 'week' ? weekStart(midnight(date)) : midnight(date)
  const days = Array.from({ length: view === 'week' ? 7 : AGENDA_DAYS }, (_, i) => addDays(first, i))
  const from = first.getTime()
  const to = addDays(first, days.length).getTime()
  // All-day events are on UTC days, which may start before the local ones.
  useEffect(() => calendar.show(from - DAY, to + DAY), [calendar, from, to])
  useEffect(() => {
    calendar.open(opened)
    return () => calendar.open(null)
  }, [calendar, opened])

  const go = (target: Date, inView = view) => route(calendarPath(inView, dateString(target)))
  const closed = calendarPath(view, date)
  useKeymap({
    next: () => go(addDays(first, view === 'week' ? 7 : AGENDA_DAYS)),
    previous: () => go(addDays(first, view === 'week' ? -7 : -AGENDA_DAYS)),
    today: () => go(new Date()),
    weekView: () => route(calendarPath('week', date, opened ?? undefined)),
    agendaView: () => route(calendarPath('agenda', date, opened ?? undefined)),
    ...(opened !== null && { up: () => route(closed) }),
  })

  const events = calendar.events.value
  const title = `${view === 'week' ? 'Week of' : 'From'} ${dateString(first)}`
  useTitle(title, 'Calendar')
  const link = (event: CalendarEvent) => calendarPath(view, date, event.id)
  return (
    <section class={styles.calendar} aria-label="Calendar">
      <header class={styles.header}>
        <h1>{title}</h1>
        <nav class={styles.controls}>
          <a href={calendarPath(view, dateString(addDays(first, view === 'week' ? -7 : -AGENDA_DAYS)))}>‹</a>
          <a href={calendarPath(view, today())}>Today</a>
          <a href={calendarPath(view, dateString(addDays(first, view === 'week' ? 7 : AGENDA_DAYS)))}>›</a>
          <a href={calendarPath('week', date)} aria-current={view === 'week' ? 'page' : undefined}>
            Week
          </a>
          <a href={calendarPath('agenda', date)} aria-current={view === 'agenda' ? 'page' : undefined}>
            Agenda
          </a>
        </nav>
      </header>
      <div class={styles.body}>
        {events === null ? (
          <p class={styles.placeholder}>Loading…</p>
        ) : view === 'week' ? (
          <Week days={days} events={events} link={link} opened={opened} />
        ) : (
          <Agenda days={days} events={events} link={link} opened={opened} />
        )}
        {opened !== null && <EventPane details={calendar.opened.value} close={closed} />}
      </div>
    </section>
  )
}

function Marks(props: { event: CalendarEvent }) {
  const { event } = props
  if (event.exception) return <span title="An occurrence changed from its series"> ↻*</span>
  if (event.recurring) return <span title="Recurring"> ↻</span>
  return null
}

const RESPONSES: Record<EventResponse, string | null> = {
  none: null,
  organizer: 'You organize it',
  tentativelyAccepted: 'Tentatively accepted',
  accepted: 'Accepted',
  declined: 'Declined',
  notResponded: 'Not responded',
}

const SHOWN_AS: Record<ShowAs, string> = {
  free: 'Free',
  tentative: 'Tentative',
  busy: 'Busy',
  oof: 'Out of office',
  workingElsewhere: 'Working elsewhere',
  unknown: 'Unknown',
}

/** What the event's status is, in words: cancelled, and the user's answer. */
function status(event: CalendarEvent): string {
  return [event.cancelled && 'Cancelled', RESPONSES[event.response], `shown as ${SHOWN_AS[event.show_as]}`]
    .filter(Boolean)
    .join(' · ')
}

/** The event's classes, by its status, as Outlook shows them: cancelled
 * struck through, declined faded, unanswered or tentative hatched, free
 * hollow. */
function eventClass(event: CalendarEvent, opened: number | null): string {
  return [
    styles.event,
    event.cancelled && styles.cancelled,
    event.response === 'declined' && styles.declined,
    (event.response === 'notResponded' || event.response === 'tentativelyAccepted' || event.show_as === 'tentative') &&
      styles.tentative,
    event.show_as === 'free' && styles.free,
    event.id === opened && styles.opened,
  ]
    .filter(Boolean)
    .join(' ')
}

function tooltip(event: CalendarEvent): string {
  return [event.subject, event.location, status(event)].filter(Boolean).join('\n')
}

type Link = (event: CalendarEvent) => string

function Week(props: { days: Date[]; events: readonly CalendarEvent[]; link: Link; opened: number | null }) {
  const now = useNow()
  const week = useRef<HTMLDivElement>(null)
  const first = props.days[0].getTime()
  const last = addDays(props.days[props.days.length - 1], 1).getTime()
  const thisWeek = first <= now && now < last
  // Opens at the working day, or, this week, at an hour before now: below
  // the heads, which stay, the hours start where the week does.
  useLayoutEffect(() => {
    const hour = thisWeek ? Math.max(0, new Date().getHours() - 1) : WORKDAY_START
    // A little above it, so that its tick shows.
    week.current!.scrollTop = hour * HOUR_PX - HOUR_PX / 4
  }, [first, thisWeek])
  return (
    <div class={styles.week} ref={week} style={{ '--hour': `${HOUR_PX}px` }}>
      <div class={styles.corner} />
      <ol class={styles.gutter} aria-hidden="true" style={{ height: `${24 * HOUR_PX}px` }}>
        {Array.from({ length: 23 }, (_, i) => (
          <li key={i} style={{ top: `${(i + 1) * HOUR_PX}px` }}>{`${String(i + 1).padStart(2, '0')}:00`}</li>
        ))}
      </ol>
      {props.days.map((day, column) => {
        const on = props.events.filter((e) => isOn(e, day))
        const start = day.getTime()
        const isToday = start <= now && now < addDays(day, 1).getTime()
        return (
          <section
            key={start}
            class={isToday ? `${styles.day} ${styles.today}` : styles.day}
            style={{ gridColumn: column + 2 }}
            aria-label={dayName(day)}
          >
            <h2 title={dayName(day)} aria-current={isToday ? 'date' : undefined}>
              {shortDayName(day)}
            </h2>
            <ul class={styles.allDay} aria-label="All day">
              {on
                .filter((e) => e.all_day)
                .map((e) => (
                  <li key={e.id}>
                    <a class={eventClass(e, props.opened)} href={props.link(e)} title={tooltip(e)} onMouseDown={keepFocus}>
                      <span class={styles.subject}>{e.subject}</span>
                      <Marks event={e} />
                    </a>
                  </li>
                ))}
            </ul>
            <div class={styles.hours} style={{ height: `${24 * HOUR_PX}px` }}>
            <ul aria-label="Timed">
              {laidOut(
                on.filter((e) => !e.all_day),
                day,
              ).map(({ event: e, top, height, lane, lanes }) => (
                <li
                  key={e.id}
                  style={{
                    top: `${top}px`,
                    height: `${height}px`,
                    left: `${(lane / lanes) * 100}%`,
                    width: `${100 / lanes}%`,
                  }}
                >
                  <a
                    class={[eventClass(e, props.opened), height < SHORT_PX && styles.short].filter(Boolean).join(' ')}
                    href={props.link(e)}
                    title={tooltip(e)}
                    onMouseDown={keepFocus}
                  >
                    <time>{time(e.start)}</time> <span class={styles.subject}>{e.subject}</span>
                    <Marks event={e} />
                    {e.location && <span class={styles.location}> · {e.location}</span>}
                  </a>
                </li>
              ))}
            </ul>
            {isToday && (
              <div class={styles.now} style={{ top: `${((now - start) / HOUR) * HOUR_PX}px` }} title={`Now, ${time(now)}`} />
            )}
            </div>
          </section>
        )
      })}
    </div>
  )
}

/** Where the day's timed events go: by time, side by side where they
 * overlap. */
function laidOut(events: CalendarEvent[], day: Date) {
  const start = day.getTime()
  const end = addDays(day, 1).getTime()
  const px = (ms: number) => ((ms - start) / HOUR) * HOUR_PX
  const placed: { event: CalendarEvent; top: number; height: number; lane: number; lanes: number }[] = []
  // Clusters of events that overlap one another, each laid out in lanes.
  let cluster: typeof placed = []
  let clusterEnd = -Infinity
  let laneEnds: number[] = []
  const close = () => {
    for (const p of cluster) p.lanes = laneEnds.length
    placed.push(...cluster)
    cluster = []
    laneEnds = []
  }
  for (const event of [...events].sort((a, b) => a.start - b.start || b.end - a.end)) {
    const from = Math.max(event.start, start)
    const to = Math.min(Math.max(event.end, event.start + 15 * 60_000), end)
    if (from >= clusterEnd) close()
    let lane = laneEnds.findIndex((laneEnd) => laneEnd <= from)
    if (lane === -1) lane = laneEnds.push(to) - 1
    else laneEnds[lane] = to
    clusterEnd = Math.max(clusterEnd, to)
    cluster.push({ event, top: px(from), height: Math.max(px(to) - px(from), 16), lane, lanes: 1 })
  }
  close()
  return placed
}

function Agenda(props: { days: Date[]; events: readonly CalendarEvent[]; link: Link; opened: number | null }) {
  const days = props.days
    .map((day) => ({ day, events: props.events.filter((e) => isOn(e, day)) }))
    .filter((d) => d.events.length > 0)
  if (days.length === 0) return <p class={styles.placeholder}>Nothing on.</p>
  return (
    <div class={styles.agenda}>
      {days.map(({ day, events }) => (
        <section key={day.getTime()} aria-label={dayName(day)}>
          <h2 aria-current={dateString(day) === today() ? 'date' : undefined}>{dayName(day)}</h2>
          <ul>
            {events.map((e) => (
              <li key={e.id}>
                <a class={`${styles.agendaEvent} ${eventClass(e, props.opened)}`} href={props.link(e)} title={tooltip(e)} onMouseDown={keepFocus}>
                  <span class={styles.when}>{e.all_day ? 'All day' : `${time(e.start)}–${time(e.end)}`}</span>
                  <span>
                    <span class={styles.subject}>{e.subject}</span>
                    <Marks event={e} />
                    {e.location && <span class={styles.location}> · {e.location}</span>}
                  </span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  )
}

/** When the event is, in words. An all-day event's are UTC dates. */
function when(event: CalendarEvent): string {
  if (event.all_day) {
    const utcDate = (ms: number) => new Date(ms).toISOString().slice(0, 10)
    const last = utcDate(event.end - DAY)
    const first = utcDate(event.start)
    return first === last ? `${first}, all day` : `${first} – ${last}, all day`
  }
  const start = new Date(event.start)
  if (dateString(start) === dateString(new Date(event.end - 1))) {
    return `${dayName(start)}, ${time(event.start)}–${time(event.end)}`
  }
  return `${dateTime(event.start)} – ${dateTime(event.end)}`
}

const ANSWERS: Record<EventResponse, string> = {
  none: '',
  organizer: 'organizer',
  tentativelyAccepted: 'tentative',
  accepted: 'accepted',
  declined: 'declined',
  notResponded: 'not responded',
}

const ANSWER_CLASSES: Partial<Record<EventResponse, string>> = {
  accepted: styles.accepted,
  declined: styles.refused,
}

function AttendeeItem(props: { attendee: Attendee }) {
  const { person, kind, response } = props.attendee
  return (
    <li title={person.address ?? undefined}>
      {person.name}
      {kind !== 'required' && <span class={styles.muted}> ({kind})</span>}
      {ANSWERS[response] && <span class={ANSWER_CLASSES[response]}> · {ANSWERS[response]}</span>}
    </li>
  )
}

/** All the calendar has of the event opened, beside the calendar. */
function EventPane(props: { details: EventDetails | 'gone' | null; close: string }) {
  const { details } = props
  const event = details !== null && details !== 'gone' ? details.event : null
  return (
    <aside class={styles.pane} aria-label="Event">
      <header class={styles.paneHeader}>
        <h2 class={event?.cancelled ? styles.cancelled : undefined}>{event?.subject ?? ''}</h2>
        <a href={props.close} aria-label="Close event" title="Close (Esc)" onMouseDown={keepFocus}>
          ×
        </a>
      </header>
      {details === null ? (
        <p class={styles.muted}>Loading…</p>
      ) : details === 'gone' ? (
        <p class={styles.muted}>This event is no longer on the calendar.</p>
      ) : (
        <Details details={details} />
      )}
    </aside>
  )
}

function Details(props: { details: EventDetails }) {
  const { event, organizer, attendees, join_url, web_link } = props.details
  return (
    <>
      <p>
        {when(event)}
        <Marks event={event} />
      </p>
      {event.cancelled && (
        <p class={styles.cancelledNote} role="status">
          Cancelled by its organizer
        </p>
      )}
      {join_url && (
        <a class={styles.join} href={join_url} target="_blank" rel="noopener noreferrer">
          Join online meeting
        </a>
      )}
      <dl class={styles.facts}>
        {event.location && (
          <>
            <dt>Where</dt>
            <dd>{event.location}</dd>
          </>
        )}
        {organizer && (
          <>
            <dt>Organizer</dt>
            <dd title={organizer.address ?? undefined}>{organizer.name}</dd>
          </>
        )}
        {RESPONSES[event.response] && (
          <>
            <dt>You</dt>
            <dd>{RESPONSES[event.response]}</dd>
          </>
        )}
        <dt>Shown as</dt>
        <dd>{SHOWN_AS[event.show_as]}</dd>
      </dl>
      {attendees.length > 0 && (
        <section aria-label="Attendees">
          <h3>Attendees ({attendees.length})</h3>
          <ul class={styles.attendees}>
            {attendees.map((a, i) => (
              <AttendeeItem key={i} attendee={a} />
            ))}
          </ul>
        </section>
      )}
      <Body details={props.details} />
      {web_link && (
        <a class={styles.external} href={web_link} target="_blank" rel="noopener noreferrer">
          Open in Outlook
        </a>
      )}
      <details class={styles.raw}>
        <summary>Provider's JSON</summary>
        <pre>{props.details.raw}</pre>
      </details>
    </>
  )
}

function Body(props: { details: EventDetails }) {
  const { body_html, body_text, remote_images } = props.details
  if (body_html !== null) {
    return (
      <div class={styles.description}>
        <HtmlBody html={body_html} fold={false} hosts={remote_images} shown={new Set()} rules={[]} pixels={0} />
      </div>
    )
  }
  if (body_text?.trim()) {
    return (
      <p class={styles.text}>
        {linkify(body_text).map((part, i) =>
          part.href === null ? (
            part.text
          ) : (
            <a key={i} href={part.href} target="_blank" rel="noopener noreferrer">
              {part.text}
            </a>
          ),
        )}
      </p>
    )
  }
  return null
}
