import { signal } from '@preact/signals'
import { render, screen, waitFor, within } from '@testing-library/preact'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import { Client } from '../stores/client'
import { FakeServer } from '../test/server'
import { App } from './App'
import { today } from './Calendar'

let server: FakeServer
let client: Client

// The tests run in New York (vite.config.ts), where November is UTC−5.
beforeEach(() => {
  server = new FakeServer()
  server.addM365Account('work', [])
  server.addEvent('work', 'Standup', '2026-11-10T15:00:00Z', '2026-11-10T15:15:00Z', { recurring: true })
  server.addEvent('work', 'Review', '2026-11-10T15:00:00Z', '2026-11-10T16:00:00Z', { location: 'Room 4' })
  server.addEvent('work', 'Lunch', '2026-11-10T17:00:00Z', '2026-11-10T18:00:00Z')
  server.addEvent('work', 'Offsite', '2026-11-12T00:00:00Z', '2026-11-13T00:00:00Z', { all_day: true })
  server.addEvent('work', 'Late call', '2026-11-16T03:00:00Z', '2026-11-16T04:00:00Z')
  client = new Client(server)
  server.attach(client)
})

async function start(path: string) {
  history.replaceState(null, '', path)
  render(<App client={client} status={signal('open')} />)
  client.reset()
  await screen.findByRole('region', { name: 'Calendar' })
  return userEvent.setup()
}

const day = (name: string) => screen.getByRole('region', { name })
const timed = (name: string) => within(within(day(name)).getByRole('list', { name: 'Timed' })).queryAllByRole('listitem')
const allDay = (name: string) =>
  within(within(day(name)).getByRole('list', { name: 'All day' }))
    .queryAllByRole('listitem')
    .map((e) => e.textContent)

describe('calendar', () => {
  it('shows the week from Monday, at local times', async () => {
    await start('/calendar/week/2026-11-11')
    expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('Week of 2026-11-09')
    await waitFor(() => expect(timed('Tuesday 2026-11-10')).toHaveLength(3))
    const [review, standup, lunch] = timed('Tuesday 2026-11-10')
    expect(standup.textContent).toBe('10:00 Standup ↻')
    expect(review.textContent).toBe('10:00 Review · Room 4')
    expect(lunch.textContent).toBe('12:00 Lunch')
    // Standup and Review overlap, so are side by side; Lunch has the day
    // to itself.
    expect([standup.style.left, standup.style.width]).toEqual(['50%', '50%'])
    expect([review.style.left, review.style.width]).toEqual(['0%', '50%'])
    expect(lunch.style.width).toBe('100%')
    expect(lunch.style.top).toBe(`${12 * 40}px`)
    // An all-day event is on its date, though that starts earlier in UTC.
    expect(allDay('Thursday 2026-11-12')).toEqual(['Offsite'])
    expect(allDay('Wednesday 2026-11-11')).toEqual([])
    expect(allDay('Friday 2026-11-13')).toEqual([])
    // 03:00 UTC on Monday is Sunday evening here.
    expect(timed('Sunday 2026-11-15').map((e) => e.textContent)).toEqual(['22:00 Late call'])
  })

  it('moves by week with j and k, to today with t, and between views with a and w', async () => {
    const user = await start('/calendar/week/2026-11-11')
    await user.keyboard('j')
    expect(location.pathname).toBe('/calendar/week/2026-11-16')
    await screen.findByRole('heading', { name: 'Week of 2026-11-16' })
    expect(timed('Monday 2026-11-16')).toEqual([])
    await user.keyboard('kk')
    expect(location.pathname).toBe('/calendar/week/2026-11-02')
    await user.keyboard('a')
    expect(location.pathname).toBe('/calendar/agenda/2026-11-02')
    await screen.findByRole('heading', { name: 'From 2026-11-02' })
    await waitFor(() => expect(screen.getByRole('region', { name: 'Tuesday 2026-11-10' })).toBeTruthy())
    await user.keyboard('j')
    expect(location.pathname).toBe('/calendar/agenda/2026-11-16')
    await user.keyboard('w')
    expect(location.pathname).toBe('/calendar/week/2026-11-16')
    await user.keyboard('t')
    expect(location.pathname).toBe(`/calendar/week/${today()}`)
  })

  it('lists the days with events in the agenda', async () => {
    await start('/calendar/agenda/2026-11-10')
    const tuesday = await screen.findByRole('region', { name: 'Tuesday 2026-11-10' })
    expect(within(tuesday).getAllByRole('listitem').map((e) => e.textContent)).toEqual([
      '10:00–10:15Standup ↻',
      '10:00–11:00Review · Room 4',
      '12:00–13:00Lunch',
    ])
    expect(
      within(day('Thursday 2026-11-12'))
        .getAllByRole('listitem')
        .map((e) => e.textContent),
    ).toEqual(['All dayOffsite'])
    expect(screen.queryByRole('region', { name: 'Wednesday 2026-11-11' })).toBeNull()
  })

  it('shows what changed when a sync says the calendar did', async () => {
    await start('/calendar/week/2026-11-11')
    await waitFor(() => expect(timed('Wednesday 2026-11-11')).toEqual([]))
    server.addEvent('work', 'Planning', '2026-11-11T19:00:00Z', '2026-11-11T20:00:00Z', { exception: true })
    server.calendarSynced('work')
    await waitFor(() => expect(timed('Wednesday 2026-11-11').map((e) => e.textContent)).toEqual(['14:00 Planning ↻*']))
  })

  it('is a link in the nav, and g c away', async () => {
    const user = await start('/calendar')
    expect(location.pathname).toBe('/calendar')
    await screen.findByRole('heading', { name: /^Week of / })
    await user.click(screen.getByRole('link', { name: 'To-dos' }))
    await screen.findByRole('region', { name: 'To-dos' })
    await user.keyboard('gc')
    expect(location.pathname).toBe(`/calendar/week/${today()}`)
    expect(screen.getByRole('link', { name: 'Calendar' }).getAttribute('aria-current')).toBe('page')
  })
})
